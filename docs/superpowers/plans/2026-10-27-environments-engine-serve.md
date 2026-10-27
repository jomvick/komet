# Environments (M1 engine + M2 serve) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let one engine hold N isolated workspaces ("environments"), each backed by its own sync server, and let a VPS produce a pairing link with one command.

**Architecture:** Each environment gets its own `WorkspaceRuntime` (docs store + `DocHost` + `WorkspaceHost` + `LinkCache`) built from the same code path as today's local bootstrap. The UI-facing transport stays one IPC port; the environment is chosen once per WebSocket connection (`?env=`) and dispatched by a `ServiceRouter`, so no method or stream signature changes. The local environment is `"local"` and behaves exactly as today.

**Tech Stack:** Rust 2024, tokio, axum (sync server), tokio-tungstenite (RPC), serde, base64, rusqlite via `DocsStore`, clap.

**Spec:** `docs/superpowers/specs/2026-10-27-environments-remote-design.md`

**Scope:** This plan is M1 (engine) + M2 (server + CLI) only. M3 (UI switcher, settings page) and M4 (hardening, docs) are a second plan, written after this one lands — they depend on the RPC surface defined here. Executors must read the spec too; it carries the intent and the anti-goals.

## Global Constraints

- Local environment id is the reserved string `"local"`. `AddEnvironment`/`RemoveEnvironment` reject it.
- `environments.json` is mode 0600, written atomically (temp file + rename in the same directory).
- A remote environment runtime **never** gets `set_sessions` and never upserts a device row: the laptop is not a device in a remote environment. Commands are queued into the doc; the owning device executes them.
- The local workspace store root, `EngineProfile`, and `WorkspaceScope` are unchanged. Remote environments live under `{data_dir}/environments/{id}/`.
- `WATCH_SESSIONS` on an environment connection is filtered to chats present in that environment's registry index.
- `AddEnvironment` must reject a server whose `/env` `orgId`/`userId` matches the active profile (`AlreadySyncedHere`).
- No new dependency. Everything ships in existing crates.
- Tokens are secrets: never logged, masked in any printed status.
- Conventional Commits with a scope, e.g. `feat(engine): ...`. One commit per task.
- Verify before the branch is handed over: `cargo check --workspace --all-targets`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all`, `cargo test -p komet-proto -p komet-engine -p komet-rpc -p komet-sync-server`.

## Review Focus

- A link pasted twice, or with a trailing newline from a chat message, must not create two environments (idempotent on `(url, token)`).
- A removed environment must not resurrect after a restart, and its room clients must stop, not just be unreferenced.
- An unknown or removed `?env=` id must fail the connection, never fall back to `"local"`.
- A dead VPS must not delay engine boot or a local RPC call.
- `GET /env` on a server with a pre-existing Docker data dir must be stable across restarts (identity persisted once).

---

### Task 1: Pairing link codec + environment wire types

**Files:**
- Create: `crates/proto/src/environment.rs`
- Modify: `crates/proto/src/lib.rs` (add `mod environment; pub use environment::*;`)

**Interfaces:**
- Produces:
  - `pub struct PairPayload { pub v: u8, pub label: String, pub url: String, pub token: String }`
  - `pub struct Environment { pub id: String, pub label: String, pub url: String, pub token: String, pub namespace: Option<Namespace>, pub added_at_ms: i64 }`
  - `pub struct Namespace { pub org_id: String, pub user_id: String }`
  - `pub enum EnvError { InvalidLink, UnsupportedVersion(u8), EmptyField(&'static str) }` (thiserror, already a workspace dep)
  - `pub const PAIR_SCHEME: &str = "komet://pair/";`
  - `pub fn encode_pair_link(p: &PairPayload) -> String`
  - `pub fn decode_pair_link(raw: &str) -> Result<PairPayload, EnvError>`

- [ ] **Step 1: Write the failing tests** in `crates/proto/src/environment.rs`, `#[cfg(test)] mod tests`:

```rust
#[test]
fn pair_link_round_trips() {
    let p = PairPayload { v: 1, label: "VPS ovh".into(), url: "https://sync.example.com".into(), token: "abc123".into() };
    assert_eq!(decode_pair_link(&encode_pair_link(&p)).unwrap(), p);
}
#[test]
fn decode_trims_surrounding_whitespace() {
    let p = PairPayload { v: 1, label: "x".into(), url: "http://10.0.0.2:8787".into(), token: "t".into() };
    let raw = format!("  {}\n", encode_pair_link(&p));
    assert_eq!(decode_pair_link(&raw).unwrap(), p);
}
#[test]
fn decode_rejects_wrong_scheme_version_and_empty_fields() {
    assert_eq!(decode_pair_link("http://example.com"), Err(EnvError::InvalidLink));
    let bad_version = encode_payload(&PairPayload { v: 2, ..valid() });
    assert_eq!(decode_pair_link(&bad_version), Err(EnvError::UnsupportedVersion(2)));
    let empty = encode_payload(&PairPayload { token: "  ".into(), ..valid() });
    assert_eq!(decode_pair_link(&empty), Err(EnvError::EmptyField("token")));
}
#[test]
fn decode_rejects_non_url_and_non_http_url() {
    let not_url = encode_payload(&PairPayload { url: "nope".into(), ..valid() });
    assert_eq!(decode_pair_link(&not_url), Err(EnvError::InvalidLink));
    let ftp = encode_payload(&PairPayload { url: "ftp://x".into(), ..valid() });
    assert_eq!(decode_pair_link(&ftp), Err(EnvError::InvalidLink));
}
```

`PairPayload` derives `Debug, Clone, PartialEq, Eq, Serialize, Deserialize`; `valid()` and `encode_payload(&PairPayload) -> String` are test helpers that build the payload body without validation, so the decoder is tested, not the encoder.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p komet-proto environment`
Expected: FAIL — `decode_pair_link` not found.

- [ ] **Step 3: Implement**

`encode_pair_link` = `PAIR_SCHEME` + base64url-no-pad of serde_json bytes. `decode_pair_link` = trim, strip the scheme prefix (prefix match is case-sensitive), base64-decode, JSON-decode, then validate in this order: `v != 1` → `UnsupportedVersion(v)`; `label`, `url`, `token` each trimmed-must-be-non-empty → `EmptyField(name)`; url must parse as absolute `http`/`https` (use `url` crate if already in `komet-proto`'s deps, else check `starts_with("http://") || starts_with("https://")` and a non-empty host) → else `InvalidLink`. Store the trimmed values. `Environment` and `Namespace` derive `Debug, Clone, PartialEq, Eq, Serialize, Deserialize`.

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p komet-proto environment`
Expected: PASS (4 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/proto/src/environment.rs crates/proto/src/lib.rs
git commit -m "feat(proto): pairing link codec and environment wire types"
```

---

### Task 2: `environments.json` store

**Files:**
- Create: `crates/engine/src/environments.rs`
- Modify: `crates/engine/src/lib.rs` (add `mod environments; pub use environments::{EnvironmentStore, AddOutcome};`)

**Interfaces:**
- Produces:
  - `pub struct EnvironmentStore { path: PathBuf, envs: Vec<Environment> }`
  - `EnvironmentStore::load(data_dir: &Path) -> Result<Self, std::io::Error>` (missing file = empty)
  - `EnvironmentStore::save(&self) -> Result<(), std::io::Error>`
  - `EnvironmentStore::list(&self) -> &[Environment]`
  - `EnvironmentStore::get(&self, id: &str) -> Option<&Environment>`
  - `EnvironmentStore::upsert(&mut self, env: Environment) -> AddOutcome` where `pub enum AddOutcome { Added, Existing(String) }` (existing = same trimmed `(url, token)`, returns its id)
  - `EnvironmentStore::remove(&mut self, id: &str) -> Result<Environment, EnvStoreError>` (`Reserved` for `"local"`, `NotFound`)
  - `pub enum EnvStoreError { Reserved, NotFound }`

- [ ] **Step 1: Write the failing tests** at the bottom of `crates/engine/src/environments.rs`

```rust
#[test]
fn upsert_is_idempotent_on_url_and_token() { /* two upserts of the same url+token -> Added then Existing(same id), list len 1 */ }
#[test]
fn load_after_save_round_trips() { /* save into tempdir, reload, compare */ }
#[test]
fn file_is_private() { /* #[cfg(unix)] save then assert mode & 0o777 == 0o600 */ }
#[test]
fn remove_refuses_local_and_unknown() { /* remove("local") -> Err(Reserved); remove("nope") -> Err(NotFound) */ }
#[test]
fn upsert_keeps_trimmed_token_and_url() { /* trailing spaces/newline on url+token are trimmed by decode, but upsert must also trim before comparing */ }
```

Use `tempfile::tempdir()` (already a dev-dep of `komet-engine`; verify in `crates/engine/Cargo.toml` — if absent, use `std::env::temp_dir()` plus a uuid subdir and clean it up in the test).

- [ ] **Step 2: Run** `cargo test -p komet-engine environments::` — expect FAIL (types missing).
- [ ] **Step 3: Implement**

`load` reads `{data_dir}/environments.json` as `{ "v": 1, "environments": [Environment] }`; an unknown `v` is an error (`io::Error::other`). `save` writes `{data_dir}/environments.json.tmp`, sets mode 0600 before writing the contents, then renames over the target. `upsert` compares `url.trim_end_matches('/')` and `token.trim()`. `remove` also refuses `"local"`.
- [ ] **Step 4: Run** `cargo test -p komet-engine environments::` — expect PASS.
- [ ] **Step 5: Commit** `feat(engine): environment store`

---

### Task 3: `?env=` connection routing

**Files:**
- Modify: `crates/rpc/src/server.rs` (`serve_ws_socket`, `serve_ws_listener`)
- Modify: `crates/rpc/src/lib.rs` (export `ServiceRouter`, `SingleService`)
- Test: `crates/rpc/tests/env_routing.rs`

**Interfaces:**
- Produces:
  - `#[async_trait] pub trait ServiceRouter: Send + Sync { async fn service_for(&self, env: Option<String>) -> Result<Arc<dyn RpcService>, String>; }`
  - `pub struct SingleService(pub Arc<dyn RpcService>);` implementing `ServiceRouter` (ignores `env`)
  - `pub async fn serve_ws_listener(listener: TcpListener, router: Arc<dyn ServiceRouter>, token: Arc<str>)` — **signature change**; update every call site (grep `serve_ws_listener(`).
  - New: `pub async fn connect_ws_for(url: &str, token: &str, env: Option<&str>) -> Result<RpcClient, RpcError>` — appends `?env=<id>` (or `&env=`) to the URL and delegates to `connect_ws`.
- The query parameter is read from the WebSocket request URI inside `serve_ws_socket`; the value is passed to `service_for` verbatim (no validation here — the engine owns the map).

- [ ] **Step 1: Write the failing test** `crates/rpc/tests/env_routing.rs`: start a listener with a router that returns `method_echo` service for `Some("a")` and a different one for `None`, and asserts `connect_ws_for(url, token, Some("a")).call(METHOD, json!({}))` reaches service A while `None` reaches the default. Second test: `service_for` returning `Err("unknown env")` closes the connection before any call is answered (`connect_ws_for(...).unwrap()` then `call` returns `RpcError::Transport`/`Closed`).
- [ ] **Step 2: Run** `cargo test -p komet-rpc --test env_routing` — expect FAIL to compile.
- [ ] **Step 3: Implement** — the router is resolved **before** the WebSocket handshake completes when possible; if the handshake already completed, send `{id:0, err:{code:"unknown_env"}}`… Simpler and honest: resolve after the handshake, on the first inbound frame's connection, and drop the socket immediately with a close frame when `service_for` errs. Keep it to the close-frame path; the test asserts the call errors.
- [ ] **Step 4: Run** `cargo test -p komet-rpc` — expect PASS, existing tests unchanged (their call sites now use `SingleService`).
- [ ] **Step 5: Commit** `feat(rpc): per-connection environment routing`

---

### Task 4: Extract `WorkspaceRuntime` from engine assembly

**Files:**
- Modify: `crates/engine/src/lib.rs` (`EngineCore::assemble_with_profile_locked`, new `WorkspaceRuntime`)
- Test: `crates/engine/tests/local_profiles.rs` (existing tests must stay green)

**Interfaces:**
- Produces:
  - `pub struct WorkspaceRuntime { pub store: Arc<DocsStore>, pub doc_host: DocHost, pub workspace: WorkspaceHost }`
  - `WorkspaceRuntime::open(store_root: &Path, device_id: &str, org_id: &str, user_id: &str, default_harness: HarnessId, edge: Option<EdgeConfig>) -> Result<Self, EngineError>` — builds exactly what `assemble_with_profile_locked` builds today for those three fields, **without** `set_sessions` (the caller decides).
  - `EngineCore` keeps the three fields it has now (delegating to the runtime) so no other code changes in this task.

- [ ] **Step 1:** No new test. Instead, capture the current behavior: run `cargo test -p komet-engine --test local_profiles` and record it green **before** touching anything.
- [ ] **Step 2: Refactor** — move the `DocsStore::open`, `RunJournal::open`… (unchanged, local only), `DocHost::new`, `WorkspaceHost::open` block into `WorkspaceRuntime::open`; `EngineCore` calls it and keeps `doc_host`/`workspace` as today, still calling `doc_host.set_sessions(sessions.clone())` for the local runtime.
- [ ] **Step 3: Run** `cargo test -p komet-engine --test local_profiles` and `cargo test -p komet-engine --test device_routing` — expect PASS, no behavior change.
- [ ] **Step 4: Commit** `refactor(engine): extract per-workspace runtime assembly`

---

### Task 5: Environments in the engine + RPC surface

**Files:**
- Modify: `crates/engine/src/lib.rs` (`EngineCore` gains `envs`, router)
- Modify: `crates/engine/src/rpc.rs` (`EngineRpc` + new methods)
- Modify: `crates/rpc/src/lib.rs` (`methods::LIST_ENVIRONMENTS`, `ADD_ENVIRONMENT`, `REMOVE_ENVIRONMENT`, `WATCH_ENVIRONMENTS`, `PREVIEW_ENVIRONMENT`)
- Test: `crates/engine/tests/environments.rs` (the double-server integration test from the spec)

**Interfaces:**
- Consumes: Task 1 (`PairPayload`, `decode_pair_link`), Task 2 (`EnvironmentStore`), Task 3 (`ServiceRouter`), Task 4 (`WorkspaceRuntime`).
- Produces:
  - `EngineCore::environments() -> Vec<Environment>` (local first, id `"local"`, token redacted to `""`)
  - `EngineCore::add_environment(&self, link: &str) -> Result<Environment, EnvError>` — decode → probe `GET {url}/env` with `Authorization: Bearer {token}` (5 s timeout) → `AlreadySyncedHere` if `orgId`/`userId` equal the active profile's → `unreachable`/`unauthorized` mapped errors → persist → build runtime → `WatchEnvironments` frame.
  - `EngineCore::remove_environment(&self, id: &str, purge: bool) -> Result<(), EnvStoreError>` — drop runtime (abort its tasks), remove entry, save; `purge` deletes `{data_dir}/environments/{id}`.
  - `pub enum EnvError { Link(EnvError), Unreachable(String), Unauthorized, VersionMismatch { server: u32, client: u32 }, AlreadySyncedHere, Store(EnvStoreError) }`
  - RPC: `WatchEnvironments` (stream of `Vec<Environment>`), `ListEnvironments`, `PreviewEnvironment { link }` → `{label, url}` (decode only), `AddEnvironment { link }`, `RemoveEnvironment { id, purge }`. All are **local-connection only**: on an environment connection they return `RpcError` code `local_only`.
  - Session scoping: on an environment connection, `WATCH_SESSIONS` yields only sessions whose chat id is in that environment's registry chats.

- [ ] **Step 1: Write the failing integration test** `crates/engine/tests/environments.rs`: reuse the in-memory relay harness from `device_routing.rs`. Two `komet_sync_server` instances on loopback (spawn `komet_sync_server::serve` on an ephemeral port — bind the listener yourself and pass it, per Task 7's refactor, or use `:0` + `/health` poll). Assertions:
  - `add_environment(link_b)` returns an env with a label; `WatchChats` on the B connection lists B's spaces only; a chat created in B does not appear in the local `WatchChats` or `WatchSessions`.
  - `add_environment` twice with the same link → `Existing`-equivalent (same id, list len unchanged).
  - `add_environment` with a bad token → `EnvError::Unauthorized`, and `environments()` unchanged.
  - `add_environment` pointing at the local profile's own server → `AlreadySyncedHere`.
  - `remove_environment("local", _)` → `Reserved`; `remove_environment(b_id, false)` → local lists unaffected and B's room clients stop (assert the B registry client's disconnect via `sync_status`).
  - `service_for(Some("nope"))` → `Err`.
- [ ] **Step 2: Run** `cargo test -p komet-engine --test environments` — expect FAIL.
- [ ] **Step 3: Implement** — `EngineCore` holds `local: WorkspaceRuntime` (moved out of its three fields, which now delegate) plus `envs: RwLock<IndexMap<String, EnvHandle>>` where `EnvHandle { env: Environment, runtime: WorkspaceRuntime, links: Arc<LinkCache> }` and a `Router` struct implementing `ServiceRouter` that returns `EngineRpc`-with-env or `EngineRpc`-local. Remote runtimes are built with `WorkspaceRuntime::open(env_root, device_id, ns.org_id, ns.user_id, default_harness, Some(edge))` and **never** `set_sessions`. Boot loads `EnvironmentStore` and builds every entry with failures logged and skipped (boot must not fail on a dead VPS).
- [ ] **Step 4: Run** `cargo test -p komet-engine --test environments` — expect PASS. Then the full crate: `cargo test -p komet-engine`.
- [ ] **Step 5: Commit** `feat(engine): environments runtime and RPC surface`

---

### Task 6: `GET /env` on the sync server

**Files:**
- Modify: `crates/sync-server/src/lib.rs` (router, `AppState`, `serve`, `serve_listener`)
- Test: `crates/sync-server/src/lib.rs` `#[cfg(test)]` (existing pattern) or `crates/sync-server/tests/env.rs`

**Interfaces:**
- Produces:
  - `pub struct ServerIdentity { pub org_id: String, pub user_id: String, pub label: String }`
  - `pub fn load_or_create_identity(data_dir: &Path) -> anyhow::Result<ServerIdentity>` — `KOMET_ORG_ID`/`KOMET_USER_ID` win; otherwise read `{data_dir}/server-identity.json` (0600); otherwise generate and persist.
  - `GET /env` → `{"v":1,"label":…,"orgId":…,"userId":…,"version":env!("CARGO_PKG_VERSION")}` behind the existing bearer check.
  - `pub async fn serve_listener(listener: TcpListener, data_dir: PathBuf, token: String) -> anyhow::Result<()>`; `serve(data_dir, token, port)` binds and delegates.

- [ ] **Step 1: Write the failing tests**: `/env` with the bearer returns the configured org/user; without the bearer returns 401; identity is stable across two `load_or_create_identity` calls on the same dir; `KOMET_ORG_ID`/`KOMET_USER_ID` override the file.
- [ ] **Step 2: Run** `cargo test -p komet-sync-server` — expect FAIL.
- [ ] **Step 3: Implement.** Router gains `.route("/env", get(env_handler))`; `AppState` gains `identity: ServerIdentity`.
- [ ] **Step 4: Run** `cargo test -p komet-sync-server` — expect PASS.
- [ ] **Step 5: Commit** `feat(sync-server): expose environment descriptor`

---

### Task 6b: Registry room protocol in `komet-sync-server`

Discovered while running Task 5's integration test: the Rust sync server only
**relayed** bytes. It never answered `hello` with `state`, so no engine could join
a registry room (`protocol: registry handshake failed`) and an environment would
show an empty sidebar — the feature's whole point. Ported the Durable Object's
protocol into the server before building `komet serve` on top of it.

**Files:**
- Create: `crates/sync-server/src/registry.rs`
- Modify: `crates/sync-server/src/lib.rs` (room state, `/registry` handler)
- Test: `crates/sync-server/src/registry.rs` `#[cfg(test)]` + the engine test in Task 5

**Interfaces:**
- `RegistryRoom::open(db_path)`, `state_frame(cursor)`, `push(batch, ops) -> (ack, Option<rows>)`,
  `presence(device, at)`, `probe_ok()`; rows and `seq` in `registry_rows`/`registry_meta`
  so a restart neither resets the sequence nor loses the sidebar.
- `/registry/{org}/ws` is stateful (JSON text: `hello`→`state`, `push`→merge+`ack`,
  `presence`, `probe`, raw `ping`→`pong`); session/chat2/device rooms stay byte relays.
- Room sockets also accept `?token=` (the bearer form every sync client sends).

- [x] Step 1: Unit tests for state/restart/delta/LWW/tombstone/presence.
- [x] Step 2: `cargo test -p komet-sync-server` — PASS.
- [x] Step 3: Wire the room map + two socket paths; rows before ack ordering.
- [x] Step 4: Un-ignore `environment_rows_never_reach_the_local_workspace`; it now
  also proves a second controller receives the row from the server's `state`.
- [x] Step 5: Commit `feat(sync-server): registry room protocol`

---

### Task 7: `komet serve`

**Files:**
- Modify: `apps/komet/src/main.rs` (`Command::Serve`, dispatch)
- Create: `apps/komet/src/serve.rs`
- Modify: `apps/komet/src/daemon.rs` (parameterized unit)
- Test: `apps/komet/src/serve.rs` `#[cfg(test)]`

**Interfaces:**
- CLI: `komet serve [--public-url URL] [--port 8787] [--install-service] [--print-link]`
- `pub struct ServeConfig { pub token: String, pub org_id: String, pub user_id: String, pub port: u16, pub public_url: Option<String>, pub label: String }`
- `ServeConfig::load_or_create(data_dir: &Path) -> Result<Self, _>` — `{data_dir}/serve.json`, 0600; token generated once (same scheme as `sync-init`: 32 hex chars); `org_id`/`user_id` generated once as uuids.
- `pub fn pair_link(cfg: &ServeConfig) -> Result<String, EnvError>` — `None` public_url → `EnvError::InvalidLink`; otherwise `encode_pair_link(&PairPayload { v: 1, label: cfg.label.clone(), url: public_url, token })`.
- `pub async fn run(cfg: ServeConfig, data_dir: PathBuf) -> anyhow::Result<()>` — binds `127.0.0.1:{port}`, then `tokio::try_join!(sync_server::serve_listener(...), engine_run(...))` with the engine configured for `Development` scope: `KOMET_ORG_ID`, `KOMET_USER_ID`, `KOMET_EDGE_TOKEN` = user_id, `KOMET_SYNC_TOKEN` = token, `KOMET_EDGE_URL` = `http://127.0.0.1:{port}`.

- [ ] **Step 1: Write failing tests** in `serve.rs`: `serve_config_round_trips_and_is_private` (tempdir: create, reload, same token/org/user; mode 0600 on unix); `pair_link_requires_public_url`; `pair_link_round_trips_through_decoder`; `print_link_is_not_emitted_on_plain_restart` (assert `should_print_link(has_url, print_link_flag, first_run)` is false on a restart without the flag).
- [ ] **Step 2: Run** `cargo test -p komet --bin komet serve::` — expect FAIL. (If the binary's test target is awkward, these tests can live in `apps/komet/src/serve.rs` and run via `cargo test -p komet`.)
- [ ] **Step 3: Implement.** First run prints the one-line Caddy snippet plus the pair link; a restart prints neither unless `--print-link`. Without `--public-url`, print the token and the snippet, never a link.
- [ ] **Step 4: Run** `cargo test -p komet` and `cargo run --bin komet -- serve --help` — expect PASS and the four flags documented.
- [ ] **Step 5: Commit** `feat(cli): komet serve for headless VPS deployments`

---

### Task 8: `--install-service` + operator docs

**Files:**
- Modify: `apps/komet/src/daemon.rs` (`render_systemd_unit` gains an `ExecStart` argument list)
- Create: `docs/environments.md`
- Modify: `README.md` (one section, linking the doc)

**Interfaces:**
- `render_systemd_unit(exe: &Path, args: &[String], env: &[(String, String)]) -> String`; the existing `headless` unit passes `&["headless"]`. `komet serve --install-service` passes `["serve", "--port", …, "--public-url", …]` and persists the same env as `serve.json` (token via `Environment=` lines, not the file, so the unit works under a different home).
- The docs page covers: Caddy snippet, pairing, `--install-service` + `loginctl enable-linger`, remove/purge, and the "token = password" warning.

- [ ] **Step 1:** Extend the existing unit-rendering tests in `daemon.rs` with one asserting `serve` args land in `ExecStart=` (the current tests already assert `Environment=` escaping — follow them).
- [ ] **Step 2: Run** `cargo test -p komet` — expect FAIL.
- [ ] **Step 3: Implement** the parameterization and the docs page.
- [ ] **Step 4: Run** `cargo test -p komet`, `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] **Step 5: Commit** `docs(env): VPS serve + pairing guide`

---

## Self-review notes

- Spec coverage: environment model (T1/T2/T5), store roots (T4/T5), `?env=` transport (T3), laptop-not-a-device (T5, constraint), session scoping (T5), `/env` (T6), `serve` + link (T7), service + docs (T8). UI switcher and hardening are explicitly out of scope for this plan.
- Uncovered by design: the UI cannot show any of this until plan 2. That is intentional — M1/M2 are proven by integration tests and `komet serve`'s printed link.
- Deliberate gap: `PreviewEnvironment` has no test of its own; it shares `decode_pair_link`, covered in T1.
