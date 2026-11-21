//! Guard test for the ext-overlay design: every komet write exercised here
//! (overlay decisions, MCP registry + secret store) must land inside komet's
//! data dir — agent-owned config files stay byte-identical.
//!
//! The agent configs hashed are the REAL user files when present (the test
//! skips absent ones), which is exactly the invariant that matters: nothing
//! komet does may rewrite `~/.claude.json`, `~/.codex/config.toml`,
//! `~/.config/opencode/opencode.json`, etc.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;

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

/// One shared komet data dir for the whole test binary: `KOMET_DATA_DIR` /
/// `KOMET_EXT_OVERLAY_PATH` are process-wide, so per-test tempdirs would race
/// under parallel execution. Initialized once, seeded with a komet-owned MCP
/// registry entry (`komet-srv`) for the origin-aware read-only checks.
fn shared_dir() -> &'static TempDir {
    static DIR: std::sync::OnceLock<TempDir> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("KOMET_DATA_DIR", dir.path());
            std::env::set_var(
                "KOMET_EXT_OVERLAY_PATH",
                dir.path().join("agent-ext-overlay.json"),
            );
        }
        std::fs::write(
            dir.path().join("mcp-servers.json"),
            r#"{"version":1,"servers":[{"id":"komet-srv","name":"KometSrv","enabled":true,"transport":"stdio","command":"srv"}]}"#,
        )
        .unwrap();
        dir
    })
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

fn hash_file(path: &Path) -> Option<u64> {
    let bytes = std::fs::read(path).ok()?;
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    Some(hasher.finish())
}

/// Every agent-owned config file komet must never write. Missing files hash
/// to `None` on both sides — the assertion is equality, not existence.
fn agent_config_paths() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    [
        ".claude.json",
        ".claude/settings.json",
        ".claude/plugins/installed_plugins.json",
        ".codex/config.toml",
        ".cursor/mcp.json",
        ".copilot/mcp-config.json",
        ".copilot/config.json",
        ".config/opencode/opencode.json",
        ".config/opencode/opencode.jsonc",
    ]
    .iter()
    .map(|rel| home.join(rel))
    .collect()
}

fn snapshot(paths: &[PathBuf]) -> Vec<Option<u64>> {
    paths.iter().map(|p| hash_file(p)).collect()
}

#[tokio::test]
async fn overlay_and_mcp_writes_leave_agent_configs_untouched() {
    shared_dir();
    let paths = agent_config_paths();
    let before = snapshot(&paths);

    let (core, _data) = test_engine();
    let rpc = core.rpc_service();

    // An explicit overlay decision for a mediated agent (Claude MCP).
    call(
        &rpc,
        methods::SET_HARNESS_EXTENSION_ENABLED,
        serde_json::json!({
            "kind": "mcp_server",
            "harness": "claude-code",
            "id": "codegraph",
            "enabled": false,
        }),
    )
    .await
    .expect("claude mcp toggle accepted");

    // A komet MCP install (registry + secret store) — the "installation"
    // half of the invariant. Composio's key-save path additionally requires
    // a live backend validation, so the registry write is the covered proxy.
    call(
        &rpc,
        methods::SAVE_MCP_SERVER,
        serde_json::json!({
            "id": "guard-test",
            "name": "Guard Test",
            "transport": "stdio",
            "command": "npx",
            "args": ["-y", "guard-test-mcp"],
            "env": { "GUARD_TOKEN": "fake-secret-xyz" },
            "headers": {},
        }),
    )
    .await
    .expect("mcp save accepted");

    let after = snapshot(&paths);
    for (path, (b, a)) in paths.iter().zip(before.iter().zip(after.iter())) {
        assert_eq!(b, a, "agent config modified by komet: {}", path.display());
    }
}

#[tokio::test]
async fn unsupported_overlay_writes_are_refused() {
    shared_dir();
    let (core, _data) = test_engine();
    let rpc = core.rpc_service();

    // Cursor has no native-MCP mediation path → typed refusal, no write.
    let err = call(
        &rpc,
        methods::SET_HARNESS_EXTENSION_ENABLED,
        serde_json::json!({
            "kind": "mcp_server",
            "harness": "cursor",
            "id": "codegraph",
            "enabled": false,
        }),
    )
    .await
    .expect_err("cursor mcp toggle must be refused");
    assert!(
        matches!(err, RpcError::Unsupported(_)),
        "expected Unsupported, got {err:?}"
    );

    // Skills have no launch mediation anywhere yet → refused too.
    let err = call(
        &rpc,
        methods::SET_HARNESS_EXTENSION_ENABLED,
        serde_json::json!({
            "kind": "skill",
            "harness": "claude-code",
            "id": "demo",
            "enabled": false,
        }),
    )
    .await
    .expect_err("skill toggle must be refused until launch mediation exists");
    assert!(matches!(err, RpcError::Unsupported(_)));

    // …but clearing is always allowed (resets stale entries).
    call(
        &rpc,
        methods::SET_HARNESS_EXTENSION_ENABLED,
        serde_json::json!({
            "kind": "skill",
            "harness": "claude-code",
            "id": "demo",
            "enabled": null,
        }),
    )
    .await
    .expect("clear must succeed even on read-only rows");
}

#[tokio::test]
async fn komet_owned_mcp_is_togglable_on_acp_agents() {
    shared_dir(); // seeds `komet-srv` into the komet MCP registry
    let (core, _data) = test_engine();
    let rpc = core.rpc_service();

    // Grok has no native-MCP mediation, but komet filters its injected
    // servers at session/new — the toggle genuinely applies.
    call(
        &rpc,
        methods::SET_HARNESS_EXTENSION_ENABLED,
        serde_json::json!({
            "kind": "mcp_server",
            "harness": "grok",
            "id": "komet-srv",
            "enabled": false,
        }),
    )
    .await
    .expect("komet-owned MCP toggle must be accepted on ACP agents");

    // The same id as an EXTERNAL row on an unmediated agent stays refused.
    let err = call(
        &rpc,
        methods::SET_HARNESS_EXTENSION_ENABLED,
        serde_json::json!({
            "kind": "mcp_server",
            "harness": "cursor",
            "id": "komet-srv",
            "enabled": false,
        }),
    )
    .await
    .expect_err("external MCP on cursor must be refused");
    assert!(matches!(err, RpcError::Unsupported(_)));
}
