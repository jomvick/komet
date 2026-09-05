# MCP interne Komet (MVP1) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Expose 3 read-only Komet tools over a per-run localhost MCP endpoint and inject it into ACP + Claude agents with allowlist permissions.

**Architecture:** New `engine::mcp` layer (pure `McpCatalog` + `McpPolicy`, zero MCP-SDK dependency) plus a thin `rmcp`-based Streamable-HTTP endpoint bound to `127.0.0.1` with per-run Bearer token; harnesses inject `{url, Bearer}` at session-creation time only, never persisted.

**Tech Stack:** Rust edition 2024, tokio full, `rmcp = "3"` (features `server`, `transport-streamable-http-server`), `axum = "0.8"`, serde_json, uuid v4. New code lives in `komet-engine` + `komet-proto` + `komet-harness`.

## Global Constraints

- Local-first: endpoint binds `127.0.0.1` only, never `0.0.0.0`; token lives in memory, masked in every log.
- No secret and no full tool input ever enters the session doc or sync: Mcp inputs in doc parts keep only `SUBAGENT_MODEL_KEYS`.
- New wire fields are additive + serde-defaulted (`skip_serializing_if`), old hosts ignore them.
- Permission default: reads `allow`, everything else `ask`, explicit `deny` wins; `ask` timeout denies.
- Zero clippy warnings (`cargo clippy --workspace --all-targets`), `cargo fmt --check` clean, commit per task.
- `ToolCall::Mcp` stays display/history only; transcript rendering already exists, no rendering changes in MVP1.

---

### Task 1: Wire type `McpInjection` on `RunRequest`

**Files:**
- Modify: `crates/proto/src/agent.rs:1069` (after `permission_timeout_ms`)
- Test: in-file `#[cfg(test)]` module at bottom of `crates/proto/src/agent.rs` (repo convention, e.g. `:1795`)

**Interfaces:**
- Consumes: nothing (first task).
- Produces: `komet_proto::McpInjection { server_name: String, url: String, auth_token: String }` (camelCase serde) and `RunRequest.mcp: Option<McpInjection>` for Tasks 4, 6, 7.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn mcp_injection_round_trips_and_stays_additive() {
    let old = r#"{"prompt":"p","model":null,"reasoning":null,"cwd":".","sandbox":"workspace-write","resume":null}"#;
    let req: RunRequest = serde_json::from_str(old).unwrap();
    assert!(req.mcp.is_none());
    let json = serde_json::to_value(&req).unwrap();
    assert!(json.get("mcp").is_none());
    let req = RunRequest {
        mcp: Some(McpInjection {
            server_name: "komet".into(),
            url: "http://127.0.0.1:9/mcp/agents?callerAgentId=r1".into(),
            auth_token: "t".into(),
        }),
        ..req
    };
    let round: RunRequest =
        serde_json::from_value(serde_json::to_value(&req).unwrap()).unwrap();
    assert_eq!(round.mcp, req.mcp);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p komet-proto mcp_injection_round_trips 2>&1 | tail -5`
Expected: FAIL with "no field `mcp`" / unresolved `McpInjection`.

- [ ] **Step 3: Write minimal implementation**

```rust
/// Per-run internal MCP endpoint riding the request (host-local).
/// Additive + serde-defaulted: an old host ignores it and runs without MCP.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpInjection {
    pub server_name: String,
    pub url: String,
    pub auth_token: String,
}
```

and on `RunRequest`, after `permission_timeout_ms`:

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub mcp: Option<McpInjection>,
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p komet-proto 2>&1 | tail -3`
Expected: all PASS, including the existing `run_request_*` compat tests.

- [ ] **Step 5: Commit**

```bash
git add crates/proto/src/agent.rs
git commit -m "feat(proto): additive RunRequest.mcp injection descriptor"
```

---

### Task 2: `McpCatalog` (pure, no MCP SDK) + doc-parts sanitizer

**Files:**
- Create: `crates/engine/src/mcp/mod.rs`, `crates/engine/src/mcp/catalog.rs`
- Modify: `crates/engine/src/lib.rs` (add `pub mod mcp;`)
- Modify: sanitizer owning `sanitize_tool_call` in `crates/doc` (locate first, see step 1)
- Test: `crates/engine/src/mcp/catalog.rs` unit tests + doc crate test for Mcp sanitize

**Interfaces:**
- Consumes: `komet_proto::ToolCall`, `SUBAGENT_MODEL_KEYS` from Task 1's crate.
- Produces for Task 4/5: `McpCatalog::komet_default(host: Arc<dyn McpHost>)`, `ToolDef { name, title, description, input_schema: serde_json::Value, readonly: bool }`, `catalog.list() -> Vec<ToolDef>`, `catalog.execute(name, input) -> Result<serde_json::Value, CatalogError>`, trait `McpHost: Send + Sync { fn list_sessions(&self) -> Vec<SessionSummary>; fn session_status(&self, id: &str) -> Option<SessionSummary>; fn list_spaces(&self) -> Vec<SpaceSummary> }` with `SessionSummary { id, title, status: String }`, `SpaceSummary { id, path, name: Option<String> }` (all Serialize).

- [ ] **Step 1: Locate the sanitizer**

Run: `grep -rn "fn sanitize_tool_call" crates/doc/src/`
Expected: one hit (used via `komet_doc::sanitize_tool_call` in `crates/engine/src/sessions.rs:27`). Read that function fully before step 4.

- [ ] **Step 2: Write the failing catalog test**

```rust
#[tokio::test]
async fn catalog_lists_three_readonly_tools_and_executes() {
    let catalog = McpCatalog::komet_default(Arc::new(FakeHost));
    let names: Vec<_> = catalog.list().iter().map(|t| t.name.clone()).collect();
    assert_eq!(names, vec!["list_sessions", "get_session_status", "list_spaces"]);
    assert!(catalog.list().iter().all(|t| t.readonly));
    let v = catalog.execute("list_spaces", serde_json::json!({})).await.unwrap();
    assert_eq!(v, serde_json::json!({"spaces": [{"id": "s1", "path": "/r", "name": null}]}));
    assert!(catalog.execute("nope", serde_json::json!({})).await.is_err());
}
```

with a `FakeHost` in the test module returning one space `s1` at `/r`.

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test -p komet-engine mcp::catalog 2>&1 | tail -5`
Expected: FAIL, module `mcp` does not exist.

- [ ] **Step 4: Write minimal implementation**

`crates/engine/src/mcp/mod.rs`:

```rust
pub mod catalog;
pub mod policy;
```

`catalog.rs` (essentials — full schemas inline, no schemars dependency):

```rust
use std::collections::HashMap;
use std::sync::Arc;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct SessionSummary { pub id: String, pub title: String, pub status: String }
#[derive(Debug, Clone, Serialize)]
pub struct SpaceSummary { pub id: String, pub path: String, pub name: Option<String> }

#[async_trait::async_trait]
pub trait McpHost: Send + Sync {
    fn list_sessions(&self) -> Vec<SessionSummary>;
    fn session_status(&self, id: &str) -> Option<SessionSummary>;
    fn list_spaces(&self) -> Vec<SpaceSummary>;
}

#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub input_schema: serde_json::Value,
    pub readonly: bool,
}

fn obj_schema(props: &[(&str, &str)], required: &[&str]) -> serde_json::Value {
    let properties: serde_json::Map<_, _> = props.iter()
        .map(|(k, t)| (k.to_string(), serde_json::json!({"type": t})))
        .collect();
    serde_json::json!({"type": "object", "properties": properties,
        "required": required, "additionalProperties": false})
}

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("unknown tool: {0}")]
    Unknown(String),
    #[error("missing required argument: {0}")]
    MissingArg(String),
}

pub struct McpCatalog { tools: HashMap<&'static str, ToolDef>, host: Arc<dyn McpHost> }

impl McpCatalog {
    pub fn komet_default(host: Arc<dyn McpHost>) -> Self {
        let defs = vec![
            ToolDef { name: "list_sessions", title: "List sessions",
                description: "List recent chat sessions with id, title and status.",
                input_schema: obj_schema(&[], &[]), readonly: true },
            ToolDef { name: "get_session_status", title: "Get session status",
                description: "Return id, title and status for one session.",
                input_schema: obj_schema(&[("sessionId", "string")], &["sessionId"]), readonly: true },
            ToolDef { name: "list_spaces", title: "List spaces",
                description: "List workspace spaces with id, path and name.",
                input_schema: obj_schema(&[], &[]), readonly: true },
        ];
        Self { tools: defs.into_iter().map(|d| (d.name, d)).collect(), host }
    }

    pub fn list(&self) -> Vec<ToolDef> {
        let mut v: Vec<_> = self.tools.values().cloned().collect();
        v.sort_by_key(|t| t.name);
        v
    }

    pub async fn execute(&self, name: &str, input: serde_json::Value) -> Result<serde_json::Value, CatalogError> {
        let tool = self.tools.get(name).ok_or_else(|| CatalogError::Unknown(name.into()))?;
        match tool.name {
            "list_sessions" => Ok(serde_json::json!({"sessions": self.host.list_sessions()})),
            "get_session_status" => {
                let id = input.get("sessionId").and_then(|v| v.as_str())
                    .ok_or_else(|| CatalogError::MissingArg("sessionId".into()))?;
                Ok(self.host.session_status(id)
                    .map(|s| serde_json::json!({"session": s}))
                    .unwrap_or_else(|| serde_json::json!({"session": null})))
            }
            "list_spaces" => Ok(serde_json::json!({"spaces": self.host.list_spaces()})),
            _ => Err(CatalogError::Unknown(name.into())),
        }
    }
}
```

(`async_trait` is already a workspace dependency, used across the codebase.)

- [ ] **Step 5: Extend the doc sanitizer for Mcp inputs**

In the function found in step 1, add the Mcp arm (keeps subagent model keys so `ToolCall::subagent_model` in `crates/proto/src/agent.rs:1184` keeps working; drops everything else so tokens/prompts never reach the doc):

```rust
ToolCall::Mcp { server, tool, input } => ToolCall::Mcp {
    server: server.clone(),
    tool: tool.clone(),
    input: input.as_ref().and_then(|v| {
        let kept: serde_json::Map<String, serde_json::Value> = SUBAGENT_MODEL_KEYS
            .iter()
            .filter_map(|k| v.get(*k).map(|val| (k.to_string(), val.clone())))
            .collect();
        if kept.is_empty() { None } else { Some(serde_json::Value::Object(kept)) }
    }),
},
```

Import `SUBAGENT_MODEL_KEYS` from `komet_proto` in that file. Add test: Mcp input `{"model":"sonnet","prompt":"secret"}` sanitizes to `{"model":"sonnet"}`; input without model keys sanitizes to `None`.

- [ ] **Step 6: Run tests**

Run: `cargo test -p komet-engine mcp::catalog 2>&1 | tail -3` then the doc-crate test for sanitize.
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/engine/src/mcp crates/engine/src/lib.rs crates/doc/src
git commit -m "feat(engine): McpCatalog read-only + Mcp doc sanitize"
```

---

### Task 3: `McpPolicy` allowlist with session memo

**Files:**
- Create: `crates/engine/src/mcp/policy.rs` (wired via `mod.rs` from Task 2)
- Test: unit tests in `policy.rs`

**Interfaces:**
- Consumes: `ToolDef.readonly` from Task 2.
- Produces for Task 4: `McpPolicy::komet_default()`, `decide(server, tool, readonly) -> Decision`, `memoize(server, tool, Decision)`, `Decision { Allow, Ask, Deny }` (Clone, Copy, PartialEq, Eq).

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn default_policy_allows_reads_asks_mutations_memoizes_session() {
    let mut p = McpPolicy::komet_default();
    assert_eq!(p.decide("komet", "list_spaces", true), Decision::Allow);
    assert_eq!(p.decide("komet", "write_file", false), Decision::Ask);
    p.memoize("komet", "write_file", Decision::Allow);
    assert_eq!(p.decide("komet", "write_file", false), Decision::Allow);
    p.set_rule("komet", "write_file", Decision::Deny);
    p.memoize("komet", "write_file", Decision::Allow);
    assert_eq!(p.decide("komet", "write_file", false), Decision::Deny);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p komet-engine mcp::policy 2>&1 | tail -3`
Expected: FAIL, module `policy` does not exist.

- [ ] **Step 3: Write minimal implementation**

```rust
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision { Allow, Ask, Deny }

pub struct McpPolicy {
    rules: HashMap<(String, String), Decision>,
    session_memo: HashMap<(String, String), Decision>,
}

impl McpPolicy {
    pub fn komet_default() -> Self {
        Self { rules: HashMap::new(), session_memo: HashMap::new() }
    }
    pub fn set_rule(&mut self, server: &str, tool: &str, d: Decision) {
        self.rules.insert((server.into(), tool.into()), d);
    }
    pub fn memoize(&mut self, server: &str, tool: &str, d: Decision) {
        self.session_memo.insert((server.into(), tool.into()), d);
    }
    pub fn decide(&self, server: &str, tool: &str, readonly: bool) -> Decision {
        let key = (server.to_string(), tool.to_string());
        if let Some(d) = self.rules.get(&key) { return *d; }
        if let Some(d) = self.session_memo.get(&key) { return *d; }
        if readonly { Decision::Allow } else { Decision::Ask }
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p komet-engine mcp:: 2>&1 | tail -3`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/engine/src/mcp/policy.rs
git commit -m "feat(engine): McpPolicy allowlist with session memo"
```

---

### Task 4: Per-run localhost endpoint (rmcp Streamable HTTP + Bearer)

**Files:**
- Modify: `Cargo.toml` (`[workspace.dependencies]` += `rmcp = { version = "3", features = ["server", "transport-streamable-http-server"] }`, `axum = "0.8"`), `crates/engine/Cargo.toml` (`rmcp.workspace = true`, `axum.workspace = true`)
- Create: `crates/engine/src/mcp/server.rs`
- Test: `crates/engine/src/mcp/server.rs` tokio tests using `reqwest` (already an engine dependency) as the MCP client for handshake + auth tests

**Interfaces:**
- Consumes: `McpCatalog`, `McpPolicy`, `Decision` (Tasks 2–3); engine permission channel type (confirm exact name while wiring Task 5 — server.rs takes `permission: PermissionSink` where `type PermissionSink = Arc<dyn Fn(PermissionAsk) -> Fut>` is NOT fixed here: define `pub struct PermissionAsk { pub server: String, pub tool: String, pub summary: String }` and `pub type AskFn = Arc<dyn Fn(PermissionAsk) -> futures::future::BoxFuture<'static, bool> + Send + Sync>`; Task 5 adapts the real pending-permission map to `AskFn`).
- Produces for Task 5: `McpEndpoint::start(run_id, catalog, policy, ask: AskFn) -> Result<RunningEndpoint, EndpointError>`, `RunningEndpoint { url: String, shutdown(): (), status() -> McpStatus }`, `McpStatus { Starting, Ready, Error(String), Stopped }` (Clone, PartialEq, Debug).

- [ ] **Step 1: Check toolchain floor, add deps, read the pinned SDK docs**

Run: `cat rust-toolchain.toml` — proceed only if channel ≥ 1.88 (rmcp 3.x MSRV); if lower, `cargo add rmcp@2.2.0` instead and note the version in this plan file.
Run: `cargo add -p komet-engine rmcp --features server,transport-streamable-http-server && cargo add -p komet-engine axum && grep -A2 'name = "rmcp"' Cargo.lock | head -3 && grep -A2 'name = "axum"' Cargo.lock | head -3`
Then read `docs.rs/rmcp/<pinned-version>` pages for `ServerHandler` (method names for `list_tools`/`call_tool`), `transport::streamable_http_server::{StreamableHttpService, StreamableHttpServerConfig}` and the `counter_streamhttp` example at that release tag. Mirror those exact names below; if a name differs, use the documented one.

- [ ] **Step 2: Write the failing tests**

Test helpers (same file, `#[cfg(test)] mod tests`):

```rust
use std::sync::Arc;

struct FakeHost;
impl McpHost for FakeHost {
    fn list_sessions(&self) -> Vec<SessionSummary> { vec![] }
    fn session_status(&self, _id: &str) -> Option<SessionSummary> { None }
    fn list_spaces(&self) -> Vec<SpaceSummary> { vec![] }
}

async fn start_test_endpoint() -> RunningEndpoint {
    McpEndpoint::start(
        "run-test",
        Arc::new(McpCatalog::komet_default(Arc::new(FakeHost))),
        McpPolicy::komet_default(),
        Arc::new(|_| Box::pin(async { false })),
    ).await.unwrap()
}

async fn post(ep: &RunningEndpoint, method: &str, params: serde_json::Value, token: Option<&str>) -> (u16, serde_json::Value) {
    let client = reqwest::Client::new();
    let mut req = client.post(ep.url()).json(&serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": method, "params": params
    }));
    if let Some(t) = token {
        req = req.header("Authorization", format!("Bearer {t}"));
    }
    let resp = req.send().await.unwrap();
    let status = resp.status().as_u16();
    let body = resp.json().await.unwrap_or(serde_json::Value::Null);
    (status, body)
}
```

Tests:

```rust
#[tokio::test]
async fn endpoint_serves_tools_list_with_bearer() {
    let ep = start_test_endpoint().await;
    let token = ep.token_for_test_only().to_string();
    let (status, body) = post(&ep, "tools/list", serde_json::json!({}), Some(&token)).await;
    assert_eq!(status, 200);
    assert_eq!(body["result"]["tools"].as_array().unwrap().len(), 3);
    ep.shutdown();
}

#[tokio::test]
async fn endpoint_rejects_bad_bearer_and_redacts_token_in_status() {
    let ep = start_test_endpoint().await;
    let (status, _) = post(&ep, "tools/list", serde_json::json!({}), None).await;
    assert_eq!(status, 401);
    let (status, _) = post(&ep, "tools/list", serde_json::json!({}), Some("wrong")).await;
    assert_eq!(status, 401);
    assert!(!format!("{:?}", ep.status()).contains(&ep.token_for_test_only()));
    ep.shutdown();
}
```

(`token_for_test_only` is `#[cfg(test)]`-gated; production code never exposes the token. Exact JSON-RPC envelope per the pinned rmcp version from step 1; `tools/list` with `{}` params is stable across spec versions.)

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p komet-engine mcp::server 2>&1 | tail -5`
Expected: FAIL, module `server` does not exist.

- [ ] **Step 4: Write minimal implementation**

Shape (names per step 1 docs):

```rust
pub async fn start(run_id: &str, catalog: Arc<McpCatalog>, policy: McpPolicy, ask: AskFn)
    -> Result<RunningEndpoint, EndpointError>
```

- generate token `uuid::Uuid::new_v4().to_string()`, store only `sha256(token)` (via workspace `sha2`) for comparison;
- `axum::Router::new().nest_service("/mcp/agents", StreamableHttpService::new(factory, session_manager, config))` layered with `axum::middleware::from_fn` that (a) requires `Authorization: Bearer <token>` (constant shape: compare `sha256(presented)` to stored hash, 401 otherwise, no token in body/logs), (b) requires `callerAgentId == run_id` query param, 403 otherwise;
- bind `tokio::net::TcpListener::bind("127.0.0.1:0")`, advertise `http://127.0.0.1:{port}/mcp/agents?callerAgentId={run_id}`;
- status `Starting` → `Ready` once serving; bind/serve failures → `Error(msg)`; `shutdown()` aborts the serve task and sets `Stopped`;
- guardrails from the spec: `axum::extract::DefaultBodyLimit::max_bytes(1024 * 1024)` on the router (1 MiB payload cap) plus a `tokio::sync::Semaphore` (4 permits) around `catalog.execute` so one agent cannot fan out unbounded concurrent calls;
- tool dispatch: `list_tools` from `catalog.list()` (name/title/description/input_schema); `call_tool` → `policy.decide("komet", name, readonly)` → `Allow` runs `catalog.execute`, `Deny` returns MCP tool-error result, `Ask` awaits `ask(PermissionAsk{...})` with 60 s timeout — `false`/timeout denies with tool-error result (model-visible, recoverable; never a protocol error).

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p komet-engine mcp::server 2>&1 | tail -3`
Expected: PASS (3 tools listed, 401 without Bearer, token absent from status debug).

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock crates/engine/Cargo.toml crates/engine/src/mcp/server.rs
git commit -m "feat(engine): per-run localhost MCP endpoint with Bearer auth"
```

---

### Task 5: Engine wiring — lifecycle, `McpHost`, permission bridge, RPCs

**Files:**
- Modify: `crates/engine/src/sessions.rs` (start endpoint per run, attach `RunRequest.mcp`, shutdown at run end)
- Modify: `crates/engine/src/mcp/catalog.rs` (add engine `McpHost` impl — new file `crates/engine/src/mcp/host.rs` preferred)
- Modify: `crates/engine/src/rpc.rs` + `crates/rpc/src/*` (4 RPCs)
- Test: engine integration test for lifecycle (start → ready → stopped, token rotation on second start)

**Interfaces:**
- Consumes: everything from Tasks 1–4.
- Produces for Tasks 6–7: live `request.mcp` on every dispatched run; RPCs `list_mcp_servers`, `list_mcp_tools`, `reconnect_mcp_server`, `watch_mcp_status` for the future Settings UI.

- [ ] **Step 1: Inventory the RPC pattern to mirror**

Run: `grep -n "pub enum\|pub struct.*Request\|pub struct.*Response" crates/engine/src/rpc.rs | head -20` and `grep -n "pub enum\|Handler" crates/rpc/src/server.rs crates/rpc/src/lib.rs | head -20`
Record the exact request/response enum and handler-registration idiom; mirror the nearest list-type RPC (e.g. whatever lists spaces/sessions) for the four new RPCs. Response shapes: `list_mcp_servers -> [{name: "komet", transport: "http", status, tool_count, last_error}]` (URL included, token NEVER included); `list_mcp_tools -> [{server, name, title, description}]`; `reconnect_mcp_server -> {ok, error?}` (refuses while its run is live: "stop the run first"); `watch_mcp_status` follows the repo's existing subscription pattern.

- [ ] **Step 2: Write the failing lifecycle test**

```rust
fn deny_all() -> AskFn {
    Arc::new(|_| Box::pin(async { false }))
}

fn test_catalog() -> Arc<McpCatalog> {
    struct Empty;
    impl McpHost for Empty {
        fn list_sessions(&self) -> Vec<SessionSummary> { vec![] }
        fn session_status(&self, _id: &str) -> Option<SessionSummary> { None }
        fn list_spaces(&self) -> Vec<SpaceSummary> { vec![] }
    }
    Arc::new(McpCatalog::komet_default(Arc::new(Empty)))
}

#[tokio::test]
async fn run_endpoint_lifecycle_ready_then_stopped_with_rotation() {
    let first = McpEndpoint::start("run-1", test_catalog(), McpPolicy::komet_default(), deny_all()).await.unwrap();
    assert_eq!(first.status(), McpStatus::Ready);
    assert!(first.url().starts_with("http://127.0.0.1:"));
    let token1 = first.token_for_test_only().to_string();
    first.shutdown();
    assert_eq!(first.status(), McpStatus::Stopped);
    let second = McpEndpoint::start("run-1", test_catalog(), McpPolicy::komet_default(), deny_all()).await.unwrap();
    assert_ne!(second.token_for_test_only(), token1);
    second.shutdown();
}
```

(`token_for_test_only` is `#[cfg(test)]`-gated; production code never exposes the token.)

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test -p komet-engine run_endpoint_lifecycle 2>&1 | tail -5`
Expected: FAIL (`McpEndpoint` has no `token_for_test_only` / `url()` yet — add both in step 4).

- [ ] **Step 4: Write minimal implementation**

- `host.rs`: `EngineMcpHost { sessions: <handle to SessionsEngine state>, registry: <spaces handle> }` — read the exact field/method names from `sessions.rs` `Inner` and `registry.rs` at edit time; map to `SessionSummary/SpaceSummary`.
- `sessions.rs` dispatch path: before harness spawn, `let mcp = McpEndpoint::start(&run_id, catalog(host), McpPolicy::komet_default(), ask_fn).await` where `ask_fn` bridges to the existing harness permission channel (`RequestPermissionFn` in `crates/harness/src/lib.rs:49`: `(PermissionKind, String, Vec<PermissionChoice>) -> oneshot::Receiver<PermissionChoice>`): call it with `PermissionKind::Tool { name: format!("mcp:komet:{tool}") }`, summary string, and `vec![PermissionChoice::Allow, PermissionChoice::AllowAlways { scope: Scope::Chat }, PermissionChoice::Deny]`; map the answer — `Allow` → `true` once, `AllowAlways { scope: Scope::Chat }` → `policy.memoize("komet", tool, Decision::Allow)` then `true`, `Deny` or dropped receiver → `false` — all under `permission_timeout_ms` (default 60 s), expiry denies; attach `request.mcp = Some(McpInjection { server_name: "komet", url: mcp.url(), auth_token: <token> })`; after the run task joins, `mcp.shutdown()`.
- On endpoint `Error`: stamp a visible run error (repo rule: every dying path carries its own visible error) and continue the run without MCP.
- RPCs per step 1 inventory.

- [ ] **Step 5: Run tests**

Run: `cargo test -p komet-engine mcp 2>&1 | tail -3`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/engine/src/sessions.rs crates/engine/src/mcp/host.rs crates/engine/src/rpc.rs crates/rpc/src
git commit -m "feat(engine): MCP lifecycle per run + status RPCs"
```

---

### Task 6: Inject into ACP harnesses (covers grok/hermes/pi/opencode)

**Files:**
- Modify: `crates/harness/src/acp/mod.rs` near `:2276` (real `session/new`; discovery probes at `:853` and `:921` keep `mcpServers: []`)
- Test: extend `crates/harness/tests/acp.rs` (follow existing mock-child patterns in that file)

**Interfaces:**
- Consumes: `RunRequest.mcp` (Task 1).
- Produces: any ACP agent launched with `request.mcp.is_some()` receives the komet server in `session/new`.

- [ ] **Step 1: Learn the adapter's `mcpServers` entry shape from the installed adapter**

Run: `grep -n "adapter" crates/harness/src/adapter_install.rs | head -10` to find the install root, then `grep -rho '"mcpServers"[^}]*}\|mcpServers[^,]*' <install-root> 2>/dev/null | head -5`.
Record the exact accepted entry shape. Contract needed: HTTP variant carrying `{url, headers}` (Paseo injects `{type: "http", url, headers: {Authorization: Bearer}}` at `runtime-mcp-config.ts:45`). If the installed adapter only accepts stdio entries, stop: mark ACP injection blocked, skip to step 5 with a `tracing::warn!` + plan note, and defer to MVP2.

- [ ] **Step 2: Write the failing test**

In `crates/harness/tests/acp.rs`, following the file's existing mock-child test setup: launch with a `RunRequest` carrying `mcp: Some(McpInjection { server_name: "komet", url: "http://127.0.0.1:9/mcp/agents?callerAgentId=r", auth_token: "tok" })`, capture the `session/new` params the harness sends, assert `mcpServers` contains one entry whose URL is the injected one and whose headers carry `Bearer tok`. Keep a second case with `mcp: None` asserting `mcpServers == []` (regression guard for current behavior).

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test -p komet-harness --test acp mcp_servers_injected 2>&1 | tail -5`
Expected: FAIL (`mcpServers` is `[]`).

- [ ] **Step 4: Write minimal implementation**

At `acp/mod.rs:2276`, build the params from `request.mcp`:

```rust
let mcp_servers: Vec<serde_json::Value> = match &request.mcp {
    Some(inj) => vec![serde_json::json!({
        "type": "http",
        "url": inj.url,
        "headers": { "Authorization": format!("Bearer {}", inj.auth_token) }
    })],
    None => vec![],
};
let session_params = serde_json::json!({ "cwd": request.cwd, "mcpServers": mcp_servers });
```

(Field names per step 1 findings — if the adapter schema differs, use its names; the test in step 2 pins them.)

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test -p komet-harness --test acp 2>&1 | tail -3`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/harness/src/acp/mod.rs crates/harness/tests/acp.rs
git commit -m "feat(harness): inject komet MCP server into ACP session/new"
```

---

### Task 7: Inject into Claude harness (spike then implement; Codex/Cursor/Antigravity explicitly deferred on red)

**Files:**
- Modify: `crates/harness/src/claude/mod.rs` `build_command` (`:172`, settings value after `:268` — read first)
- Test: `crates/harness/tests/claude.rs` command-line assertion test (follow file patterns)

**Interfaces:**
- Consumes: `RunRequest.mcp` (Task 1).
- Produces: Claude runs with `request.mcp` see the komet server without any persistent user-config change.

- [ ] **Step 1: Spike the injection mechanism (5 minutes, record outcome here)**

Run: `claude --help | grep -i -B2 -A2 mcp | head -30` and read `claude/mod.rs:268-310` (what value follows `cmd.arg("--settings")`).
Decision table: (a) a `--mcp-config <file>`-style flag exists → write `{"mcpServers": {"komet": {"type": "http", "url": ..., "headers": {...}}}}` to a `tempfile` (already a workspace dep) and pass the flag; (b) `--settings` JSON accepts an `mcpServers` key → merge into the existing settings map; (c) neither → stop, leave Claude un-injected, note it here and defer to MVP2. Implement only the winning branch.

- [ ] **Step 2: Write the failing test**

Assert the spawned command contains the injection (flag + temp-file path, or settings JSON containing `komet` with the run URL) when `request.mcp` is set, and contains neither when it is `None`. Model on the existing `claude.rs` command-shape tests.

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test -p komet-harness --test claude mcp_injection 2>&1 | tail -5`
Expected: FAIL.

- [ ] **Step 4: Write minimal implementation**

Temp config file approach (branch (a)):

```rust
if let Some(inj) = &request.mcp {
    let mut cfg = tempfile::Builder::new().prefix("komet-mcp-").suffix(".json")
        .tempfile()?; // std::io::Error converts via HarnessError::Io (#[from], lib.rs:36)
    serde_json::to_writer(&mut cfg, &serde_json::json!({
        "mcpServers": { &inj.server_name: {
            "type": "http", "url": inj.url,
            "headers": { "Authorization": format!("Bearer {}", inj.auth_token) }
        }}
    })).map_err(|e| HarnessError::Protocol(e.to_string()))?;
    let path = cfg.path().to_owned();
    std::mem::forget(cfg); // lives for the child run; deleted with the run tempdir cleanup
    cmd.arg("--mcp-config");
    cmd.arg(path);
}
```

(If branch (b) wins the spike instead: merge `mcpServers` into the existing settings map AND delete any pre-existing `"komet"` key first — strip-before-inject, the Paseo `stripInternalPaseoMcpServer` rule — so a stale user entry can never shadow the per-run token. Tempfile lifetime must cover the child: tie it to the run handle, not `forget`, if the harness has a run-scoped owner — check at edit time.)

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test -p komet-harness --test claude 2>&1 | tail -3`
Expected: PASS.

- [ ] **Step 6: End-to-end handshake + full gate**

Run: start a real `komet headless` mock-harness run if available, else run `cargo test --workspace 2>&1 | tail -5`, then `cargo clippy --workspace --all-targets 2>&1 | tail -3` (must be zero warnings) and `cargo fmt --check`.
Also verify secret hygiene: `grep -rn "Bearer" crates/ui/src/transcript.rs crates/doc/src/ | head` must show nothing, and an Mcp tool call round-trips through `sanitize_tool_call` without its input (Task 2 test covers this; re-run it).

- [ ] **Step 7: Commit**

```bash
git add crates/harness/src/claude/mod.rs crates/harness/tests/claude.rs
git commit -m "feat(harness): inject komet MCP server into Claude runs"
```

---

## Out of scope (MVP2+)

Codex/Cursor/Antigravity injection if their spikes stay red; `add/update/remove/set_enabled` RPCs; external `stdio` Supervisor with backoff; Settings UI; HTTP/SSE remotes; resources/prompts; config sync. Any red spike above is recorded in this file and becomes the MVP2 headliner — never silently expanded here.
