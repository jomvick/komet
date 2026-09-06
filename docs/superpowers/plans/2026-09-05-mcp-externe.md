# MCP externe pour les agents Komet Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Permettre à un agent Komet d'utiliser des MCP externes (GitHub/Notion/Slack/DB/navigateur) via `mcpServers` configurés dans Komet et injectés au provider, sans exposer les secrets.

**Architecture:** `McpServerConfig` canonique + `McpSecretStore` local → `McpRegistry` (load/save/resolve) → résolution au `RunRequest` via `chat.mcp_server_ids` → injection harness (`acp`/`claude`/`codex`/`cursor`) avec matrice capacités → `McpPolicy` (server+tool → allow|ask|deny) → RPC sans secrets → UI Settings→MCP Servers. Interne `komet` MCP reste séparé (`RunRequest.mcp`).

**Tech Stack:** Rust, Tokio, Serde, rmcp, Rusqlite (store), GPUI (UI), JSON-RPC IPC

## Global Constraints

- `McpServerConfig { id, name, enabled, transport: Stdio|Http|Sse, command, args, url, headers, env, always_load }` — verbatim
- Ne jamais mettre tokens dans `RunRequest`, documents Loro ou sync room — secrets restent dans `McpSecretStore` local
- Transmettre config complète uniquement au lancement provider via `mcpServers`
- Garder `RunRequest.mcp` (serveur interne Komet) séparé des serveurs externes
- `McpPolicy: server + tool → allow|ask|deny`, inconnu → `ask`, `deny` prioritaire, `allow` mémorisable session
- RPC réponses ne retournent jamais token/valeur headers/secrets/env sensible
- Masquer headers/tokens/variables dans logs (tracing)
- Provider reste responsable connexion pendant run; client MCP séparé uniquement pour `Test`/`tools/list` UI

---

## File Structure

```
crates/proto/src/mcp.rs                 # NEW: McpServerConfig, McpTransport, McpServerRef (public API)
crates/engine/src/mcp/config.rs         # NEW: McpServerConfig canonique + serde + validation
crates/engine/src/mcp/secrets.rs        # NEW: McpSecretStore (keychain/file, chiffrement at-rest)
crates/engine/src/mcp/registry.rs       # NEW: McpRegistry (sqlite/json, CRUD, resolve)
crates/engine/src/mcp/discovery.rs      # NEW: client MCP temporaire pour tools/list + status
crates/engine/src/mcp/mod.rs            # MOD: réexporte config/registry/secrets/discovery
crates/engine/src/sessions.rs           # MOD: ChatConfig.mcp_server_ids, resolve avant RunRequest
crates/harness/src/acp/mod.rs           # MOD: injecte mcpServers externes (fusion avec interne)
crates/harness/src/claude/mod.rs        # MOD: --mcp-config fusion, déduplication
crates/harness/src/capabilities.rs      # NEW: matrice provider → supports dynamic MCP
crates/engine/src/mcp/policy.rs         # MOD: étendre McpPolicy server+tool allow|ask|deny
crates/rpc/src/lib.rs                   # MOD: méthodes ListMcpServers/Get/Save/Delete/SetEnabled/Test/ListMcpTools/WatchMcpStatus
crates/engine/src/rpc.rs                # MOD: dispatch RPC MCP (sans secrets)
crates/ui/src/settings/mcp.rs           # NEW: page Settings → MCP Servers
crates/ui/src/settings/mod.rs           # MOD: route page
tests/mcp_roundtrip.rs                  # NEW: round-trip config sans secrets
```

---

### Task 1: McpServerConfig + McpSecretStore

**Files:**
- Create: `crates/engine/src/mcp/config.rs`
- Create: `crates/engine/src/mcp/secrets.rs`
- Modify: `crates/engine/src/mcp/mod.rs:1-4`
- Modify: `crates/proto/src/lib.rs` (re-export type)
- Test: `crates/engine/src/mcp/tests_config.rs`

**Interfaces:**
- Consumes: `serde`, `rusqlite`, `dirs`
- Produces: `McpServerConfig { id, name, enabled, transport, command, args, url, headers, env, always_load }`, `McpSecretStore::resolve(&self, id) -> HashMap<String,String>`, `McpSecretStore::set_secret(id, key, value)`

- [ ] **Step 1: Write failing test for config validation**

```rust
// crates/engine/src/mcp/tests_config.rs
#[test]
fn rejects_stdio_without_command() {
    let cfg = McpServerConfig { id: "gh".into(), name: "GitHub".into(), enabled: true, transport: McpTransport::Stdio, command: None, args: vec![], url: None, headers: Default::default(), env: Default::default(), always_load: false };
    assert!(cfg.validate().is_err());
}
#[test]
fn http_requires_url() {
    let cfg = McpServerConfig { id: "n".into(), name: "N".into(), enabled: true, transport: McpTransport::Http, command: None, args: vec![], url: None, headers: Default::default(), env: Default::default(), always_load: false };
    assert!(cfg.validate().is_err());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p komet-engine mcp::tests_config -- --nocapture`
Expected: FAIL `cannot find McpServerConfig`

- [ ] **Step 3: Write minimal implementation**

```rust
// crates/engine/src/mcp/config.rs
use serde::{Deserialize, Serialize};
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum McpTransport { Stdio, Http, Sse }
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct McpServerConfig {
    pub id: String, pub name: String, pub enabled: bool,
    pub transport: McpTransport,
    pub command: Option<String>, pub args: Vec<String>,
    pub url: Option<String>,
    pub headers: std::collections::HashMap<String,String>,
    pub env: std::collections::HashMap<String,String>,
    pub always_load: bool,
}
impl McpServerConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        match self.transport {
            McpTransport::Stdio => { if self.command.is_none() { anyhow::bail!("stdio requires command") } }
            McpTransport::Http | McpTransport::Sse => { if self.url.is_none() { anyhow::bail!("http/sse requires url") } }
        }
        if self.id.is_empty() || self.name.is_empty() { anyhow::bail!("id/name required") }
        Ok(())
    }
    pub fn public_view(&self) -> PublicMcpServerConfig { /* sans headers/env valeurs */ }
}
```

Secrets store: fichier `~/.komet/mcp-secrets.json` chmod 600, ou OS keychain fallback, API `set_secret/get_resolved_headers`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p komet-engine mcp::tests_config -- --nocapture`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/engine/src/mcp/config.rs crates/engine/src/mcp/secrets.rs crates/engine/src/mcp/mod.rs
git commit -m "feat(mcp): McpServerConfig canonique + McpSecretStore sans fuite secrets"
```

---

### Task 2: McpRegistry

**Files:**
- Create: `crates/engine/src/mcp/registry.rs`
- Modify: `crates/engine/src/mcp/mod.rs`
- Test: `crates/engine/src/mcp/tests_registry.rs`

**Interfaces:**
- Consumes: `McpServerConfig`, `McpSecretStore`
- Produces: `McpRegistry::load(data_dir)`, `save`, `add/update/delete/set_enabled`, `list_public() -> Vec<PublicMcpServerConfig>`, `resolve(ids: &[String]) -> Vec<ResolvedMcpServer>`

```rust
pub struct ResolvedMcpServer { pub config: McpServerConfig, pub resolved_headers: HashMap<String,String>, pub resolved_env: HashMap<String,String> }
impl McpRegistry {
    pub fn resolve(&self, ids: &[String], store: &McpSecretStore) -> Vec<ResolvedMcpServer> // filtre enabled, valide, résout secrets
}
```

- [ ] **Step 1: Write failing test**

```rust
#[test]
fn registry_persists_and_masks_secrets() {
    let dir = tempfile::tempdir().unwrap();
    let mut reg = McpRegistry::load(dir.path()).unwrap();
    reg.save(&McpServerConfig { id: "gh".into(), name: "GitHub".into(), enabled: true, transport: McpTransport::Stdio, command: Some("npx".into()), args: vec!["mcp-github".into()], url: None, headers: Default::default(), env: [("GH_TOKEN".into(),"secret".into())].into(), always_load: false }).unwrap();
    let list = reg.list_public();
    assert_eq!(list[0].id, "gh");
    assert!(list[0].headers.is_empty()); // masqué
    let reg2 = McpRegistry::load(dir.path()).unwrap();
    assert_eq!(reg2.list_public().len(), 1);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p komet-engine mcp::tests_registry -- --nocapture`
Expected: FAIL

- [ ] **Step 3: Write minimal implementation**

- Store JSON `~/.komet/mcp-servers.json` (ou SQLite `mcp_servers` table si `rusqlite` déjà utilisé). Versionnée, migration.
- `save` valide via `config.validate()`, persiste, `list_public` retourne `PublicMcpServerConfig { id, name, enabled, transport, command, args, url, always_load, has_secrets: bool }` sans valeurs.
- `resolve` filtre `enabled`, appelle `store.resolve_env/headers`, retourne erreurs masquées.
- Logs via `tracing::debug!(server_id=%id)` sans dump headers.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p komet-engine mcp::tests_registry -- --nocapture`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/engine/src/mcp/registry.rs
git commit -m "feat(mcp): McpRegistry load/save/resolve avec masquage secrets"
```

---

### Task 3: Chat/Session assignment → Run preparation

**Files:**
- Modify: `crates/engine/src/sessions.rs:1-50` (imports)
- Modify: `crates/doc/src/schema.rs` (ChatConfig.mcp_server_ids)
- Modify: `crates/engine/src/lib.rs:234` (inject McpRegistry dans EngineCore)
- Test: `crates/engine/tests/mcp_assign.rs`

**Interfaces:**
- Consumes: `McpRegistry`, `McpSecretStore`
- Produces: `ChatConfig { mcp_server_ids: Vec<String> }`, `SessionsEngine::prepare_mcp_for_run(chat_id) -> Vec<ResolvedMcpServer>`

- [ ] **Step 1: Write failing test**

```rust
#[tokio::test]
async fn run_does_not_store_secrets_in_journal() {
    let engine = test_engine().await;
    let chat = engine.create_chat_with_mcp(vec!["gh".into()]).await;
    let req = engine.build_run_request(&chat.id).await.unwrap();
    assert!(req.mcp.is_none() || !format!("{:?}", req).contains("secret"));
    // vérifie journal JSONL ne contient pas secret
    let journal = std::fs::read_to_string(journal_path(&chat.id)).unwrap();
    assert!(!journal.contains("secret"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p komet-engine --test mcp_assign -- --nocapture`
Expected: FAIL `prepare_mcp_for_run not found`

- [ ] **Step 3: Write minimal implementation**

- Ajoute `mcp_server_ids: Vec<String>` à `ChatConfig` (schema Loro, migration).
- `EngineCore` tient `Arc<McpRegistry>` + `McpSecretStore` (init dans `assemble_with_profile_locked`).
- Avant `Harness::run`, `sessions.rs` fait:
```rust
let resolved = self.mcp_registry.resolve(&chat.mcp_server_ids, &self.mcp_secrets);
let harness_mcp_servers = resolved.into_iter().map(|r| r.into_harness_config()).collect();
```
- Ne stocke jamais `resolved_headers/env` dans transcript; seulement `Public` pour UI.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p komet-engine --test mcp_assign -- --nocapture`
Expected: PASS + `rg secret journals/` vide

- [ ] **Step 5: Commit**

```bash
git add crates/engine/src/sessions.rs crates/doc/src/schema.rs crates/engine/src/lib.rs
git commit -m "feat(mcp): assignment chat→mcp_server_ids + préparation run sans secrets"
```

---

### Task 4: Injection ACP

**Files:**
- Modify: `crates/harness/src/acp/mod.rs:2290,853,921` (déjà patché pour `env: []` + mcp)
- Modify: `crates/harness/src/acp/mod.rs:2290-2305` (fusion)
- Test: `crates/harness/tests/acp_mcp_injection.rs`

**Interfaces:**
- Consumes: `Vec<ResolvedMcpServer>` via `RunRequest.mcp_external` (nouveau champ) ou fusion dans `session_params`
- Produces: `session/new` JSON avec `mcpServers` contenant à la fois interne `komet` + externes

- [ ] **Step 1: Write failing test**

```rust
#[test]
fn acp_session_params_merges_internal_and_external() {
    let internal = McpInjection { server_name: "komet".into(), url: "http://127.0.0.1/mcp".into(), auth_token: "t".into() };
    let external = ResolvedMcpServer { config: McpServerConfig { id: "gh".into(), name:"GitHub".into(), enabled:true, transport:McpTransport::Http, url:Some("https://mcp.github.com".into()), ..Default::default()}, resolved_headers: [("Authorization".into(),"Bearer x".into())].into(), resolved_env: Default::default() };
    let params = build_acp_session_params("/ws", Some(internal), vec![external]);
    assert_eq!(params["mcpServers"].as_array().unwrap().len(), 2);
    assert_eq!(params["env"], serde_json::json!([]));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p komet-harness acp_mcp_injection -- --nocapture`
Expected: FAIL

- [ ] **Step 3: Write minimal implementation**

```rust
fn build_acp_session_params(cwd: &str, internal: Option<McpInjection>, externals: Vec<ResolvedMcpServer>) -> serde_json::Value {
    let mut servers = Vec::new();
    if let Some(inj) = internal { servers.push(json!({"type":"http","url":inj.url,"headers":{"Authorization":format!("Bearer {}",inj.auth_token)}})); }
    for ext in externals {
        match ext.config.transport {
            McpTransport::Http => servers.push(json!({"type":"http","url":ext.config.url,"headers":ext.resolved_headers})),
            McpTransport::Sse => servers.push(json!({"type":"sse","url":ext.config.url,"headers":ext.resolved_headers})),
            McpTransport::Stdio => servers.push(json!({"name":ext.config.id,"command":ext.config.command,"args":ext.config.args,"env":ext.resolved_env})),
        }
    }
    json!({"cwd": cwd, "mcpServers": servers, "env": []})
}
```

Modifier `run_session` et `discover_*` pour utiliser cette fonction.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p komet-harness acp_mcp_injection -- --nocapture`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/harness/src/acp/mod.rs
git commit -m "feat(mcp): injection ACP mcpServers externe + interne"
```

---

### Task 5: Injection Claude + matrice capacités

**Files:**
- Modify: `crates/harness/src/claude/mod.rs:270-295` (build_command)
- Create: `crates/harness/src/capabilities.rs`
- Test: `crates/harness/src/claude/tests_mcp.rs`

**Interfaces:**
- Consumes: `ResolvedMcpServer`
- Produces: `fn supports_dynamic_mcp(harness: HarnessId) -> bool`, `ClaudeHarness::build_command` fusionne `mcpServers` JSON avec `always_load`

- [ ] **Step 1: Write failing test**

```rust
#[test]
fn claude_mcp_config_merges_without_duplicate() {
    let req = base_request_with_mcp(vec![resolved_github(), resolved_notion()]);
    let h = ClaudeHarness::new();
    let cmd = h.build_command(&PathBuf::from("claude"), &req);
    let cfg = extract_mcp_config(&cmd);
    assert_eq!(cfg["mcpServers"].as_object().unwrap().len(), 3); // komet + gh + notion
}
#[test]
fn capability_matrix_reports_codex_status() {
    assert_eq!(supports_dynamic_mcp(HarnessId::ClaudeCode), true);
    assert_eq!(supports_dynamic_mcp(HarnessId::Codex), false); // à confirmer
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p komet-harness claude::tests_mcp -- --nocapture`
Expected: FAIL

- [ ] **Step 3: Write minimal implementation**

- `capabilities.rs`: `pub fn supports_dynamic_mcp(id: HarnessId) -> (bool, &'static str)` → `(true, "")` pour `claude`, `opencode`, `(false, "Ce provider ne supporte pas l'injection MCP dynamique")` pour `codex`/`cursor`/autres. Utilisé par UI pour désactiver run.
- `claude/mod.rs:build_command`: fusionne `request.mcp_external` (Vec<Resolved>) en `serde_json::Map`, déduplique par `id`, supprime doublon si `url` identique, n'écrase pas fichier utilisateur sans `always_load`:
```rust
let mut servers = Map::new();
if let Some(mcp) = &request.mcp { servers.insert(mcp.server_name.clone(), json!({...})); }
for ext in externals { servers.insert(ext.config.id.clone(), ext.into_json()); }
let config = json!({"mcpServers": servers}).to_string();
cmd.args(["--mcp-config", &config]);
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p komet-harness -- --nocapture`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/harness/src/capabilities.rs crates/harness/src/claude/mod.rs
git commit -m "feat(mcp): injection Claude + matrice capacités provider"
```

---

### Task 6: Permissions MCP étendues + supervision

**Files:**
- Modify: `crates/engine/src/mcp/policy.rs`
- Create: `crates/engine/src/mcp/discovery.rs`
- Modify: `crates/engine/src/mcp/server.rs`
- Test: `crates/engine/src/mcp/tests_policy.rs`

**Interfaces:**
- Consumes: `McpServerConfig`, `rmcp::Client`
- Produces: `McpPolicy::check(server, tool) -> Allow|Ask|Deny`, `McpDiscovery::list_tools(server) -> Vec<Tool>`, `McpStatus { starting|ready|error|stopped }`

- [ ] **Step 1: Write failing test**

```rust
#[test]
fn policy_unknown_tool_asks_and_deny_wins() {
    let mut pol = McpPolicy::default(); // inconnue → ask
    assert_eq!(pol.check("github","unknown_tool"), Ask);
    pol.set("github","create_issue", Allow);
    pol.set("github","create_issue", Deny);
    assert_eq!(pol.check("github","create_issue"), Deny); // deny prioritaire
}
#[test]
fn policy_write_asks_by_default() {
    let pol = McpPolicy::default();
    assert_eq!(pol.check("github","create_issue"), Ask); // écriture
    assert_eq!(pol.check("github","list_issues"), Allow); // lecture approuvée → allow
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p komet-engine mcp::tests_policy -- --nocapture`
Expected: FAIL

- [ ] **Step 3: Write minimal implementation**

- `policy.rs`: `HashMap<(String,String), Decision>` + règles par défaut (liste blanche lecture: `list_*`, `get_*` → Allow si explicitement approuvé sinon Ask; `create_*`, `delete_*`, `exec`, `terminal` → Ask). `allow` mémorisable session via `HashSet` temporaire.
- Permission prompt `crates/engine/src/doc_host.rs` affiche `Serveur: {s} Tool: {t} Arguments: {summary_sécurisé}` (arguments tronqués/masqués, pas de token).
- `discovery.rs`: client `rmcp` éphémère (timeout 5s connexion, 10s `tools/list`), détecte `stdio` mort (exit code), ferme proprement à fin run, statut `starting→ready|error|stopped` diffusé via `tokio::watch`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p komet-engine mcp -- --nocapture`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/engine/src/mcp/policy.rs crates/engine/src/mcp/discovery.rs
git commit -m "feat(mcp): permissions server+tool + découverte/supervision statuts"
```

---

### Task 7: RPC backend

**Files:**
- Modify: `crates/rpc/src/lib.rs:35-150`
- Modify: `crates/engine/src/rpc.rs`
- Test: `crates/rpc/tests/mcp_rpc.rs`

**Interfaces:**
- Consumes: `McpRegistry`, `McpDiscovery`
- Produces: 7 RPCs: `ListMcpServers`, `GetMcpServer`, `SaveMcpServer`, `DeleteMcpServer`, `SetMcpServerEnabled`, `TestMcpServer`, `ListMcpTools`, `WatchMcpStatus`

- [ ] **Step 1: Write failing test**

```rust
#[tokio::test]
async fn rpc_list_mcp_servers_masks_secrets() {
    let client = memory_client(engine_rpc());
    let res: Value = client.call("ListMcpServers", json!({})).await.unwrap();
    assert!(res["servers"][0].get("headers").is_none());
    assert!(res["servers"][0]["has_secrets"] == true);
}
#[tokio::test]
async fn rpc_save_then_test_mcp_server() {
    let client = memory_client(engine_rpc());
    client.call("SaveMcpServer", json!({"id":"gh","name":"GitHub","transport":"stdio","command":"npx","args":["mcp"]})).await.unwrap();
    let res: Value = client.call("TestMcpServer", json!({"id":"gh"})).await.unwrap();
    assert!(res["status"] == "ready" || res["status"] == "error");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p komet-rpc mcp_rpc -- --nocapture`
Expected: FAIL `UnknownMethod ListMcpServers`

- [ ] **Step 3: Write minimal implementation**

- `crates/rpc/src/lib.rs`: ajouter constantes `LIST_MCP_SERVERS etc.` (section `methods`), déjà partiellement présent (`LIST_MCP_SERVERS`, `LIST_MCP_TOOLS` ligne 145).
- `crates/engine/src/rpc.rs`: match `method` → `McpRegistry` calls, `TestMcpServer` lance `McpDiscovery::test` (client éphémère, timeout 5s), `ListMcpTools` → `tools/list`, `WatchMcpStatus` → stream `watch::Receiver<McpStatus>`.
- Réponses: `PublicMcpServerConfig` seulement; `Test` retourne `{status, latency_ms, tools_count?, error?}` sans secrets.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p komet-engine --test rpc -- --nocapture`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/rpc/src/lib.rs crates/engine/src/rpc.rs
git commit -m "feat(mcp): RPC List/Get/Save/Delete/Test/ListTools/WatchStatus sans secrets"
```

---

### Task 8: UI Settings → MCP Servers

**Files:**
- Create: `crates/ui/src/settings/mcp.rs`
- Modify: `crates/ui/src/settings/mod.rs`
- Modify: `crates/ui/src/composer.rs` (sélection MCP session)
- Test: `crates/ui/tests/mcp_settings.rs`

**Interfaces:**
- Consumes: RPC `ListMcpServers` etc., `capabilities::supports_dynamic_mcp`
- Produces: Page `Settings → MCP Servers` + `Session MCP picker`

- [ ] **Step 1: Write failing test**

```rust
#[test]
fn mcp_settings_page_renders_masked_headers() {
    let page = render_mcp_page(vec![mock_github_server()]);
    assert!(page.contains("GitHub"));
    assert!(!page.contains("secret-token")); // masqué
    assert!(page.contains("••••"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p komet-ui mcp_settings -- --nocapture`
Expected: FAIL

- [ ] **Step 3: Write minimal implementation**

- `crates/ui/src/settings/mcp.rs`: GPUI component
  - Liste serveurs (nom, transport, statut `starting|ready|error|stopped`, dernière erreur, toggle `enabled`)
  - Formulaire ajout: `Stdio` (command/args/env), `Http`/`Sse` (url/headers), champs secrets `type=password` masqués (`••••`), bouton `Test connection` → `TestMcpServer` → affiche `tools/list` découverts, choix `always_load` + agents/sessions autorisés (`Chargement Christensen` via multiselect)
  - Avertissement si `!supports_dynamic_mcp(selected_harness)` → banner rouge au lieu de lancer run incomplet
  - `crates/ui/src/composer.rs`: champ `mcp_server_ids` multiselect (sans valeurs sensibles) dans config session, persiste via `Mutate` → `mcp_server_ids`

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p komet-ui mcp_settings -- --nocapture`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/ui/src/settings/mcp.rs crates/ui/src/composer.rs
git commit -m "feat(ui): Settings→MCP Servers + picker session sans secrets"
```

---

### Task 9: Tests end-to-end

**Files:**
- Create: `tests/mcp_e2e.rs`
- Create: `crates/engine/tests/mcp_injection.rs`
- Test: `cargo test --test mcp_e2e`

**Interfaces:**
- Consumes: Toutes les tâches 1-8
- Produces: Couverture: round-trip sans secrets, stdio mock, http mock, tools/list, timeout/crash, injection ACP/Claude, provider non-supporté, permissions, UI flow

- [ ] **Step 1: Write failing test**

```rust
#[tokio::test]
async fn e2e_mcp_stdio_mock_roundtrip() {
    let mock = MockMcpServer::stdio(vec!["echo","hi"]).start().await;
    let reg = McpRegistry::ephemeral();
    reg.save(mock.config()).unwrap();
    let chat = engine.create_chat_with_mcp(vec![mock.id()]).await;
    let events = engine.run_chat(&chat.id, "list tools").await;
    assert!(events.contains_tool("list_sessions")); // interne + externe présents
    assert!(!journal_contains_secret(&chat.id));
}
#[tokio::test]
async fn e2e_provider_unsupported_shows_error_not_run() {
    let res = engine.try_run_with_unsupported_provider(HarnessId::Codex, vec!["gh".into()]).await;
    assert!(res.is_err());
    assert!(res.unwrap_err().to_string().contains("ne supporte pas l'injection MCP"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test mcp_e2e -- --nocapture`
Expected: FAIL

- [ ] **Step 3: Write minimal implementation**

- Mock `stdio` via `rmcp` `MockServer` (process `cat` ou `node` fake), mock `http` via `axum` test server (endpoint `/mcp` répond `tools/list`).
- Tests: `config round-trip sans secrets`, `stdio mock`, `http mock`, `tools/list` découvert, `timeout 2s` + `crash stdio` → statut `error`, `injection ACP` (assert `session/new` payload contient 2 `mcpServers`), `injection Claude` (assert `--mcp-config` contient externe), `provider unsupported` → RPC erreur guidée, `permissions allow|ask|deny`, `secrets jamais dans doc`, `UI create/delete`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --test mcp_e2e -- --nocapture`
Expected: PASS (tous mocks verts)

- [ ] **Step 5: Commit**

```bash
git add tests/mcp_e2e.rs crates/engine/tests/mcp_injection.rs
git commit -m "test(mcp): e2e stdio/http, timeout, injection, permissions, sans fuite secrets"
```

---

## Self-Review Checklist

- [ ] Spec §1 config canonique → Task 1
- [ ] Spec §2 McpRegistry → Task 2
- [ ] Spec §3 préparation run → Task 3
- [ ] Spec §4 harnesses ACP/Claude/Codex matrix → Tasks 4-5
- [ ] Spec §5 permissions allow|ask|deny + UI → Task 6
- [ ] Spec §6 découverte/supervision timeouts → Task 6
- [ ] Spec §7 RPC List/Get/Save/Delete/Test/ListTools/Watch → Task 7
- [ ] Spec §8 UI Settings→MCP Servers → Task 8
- [ ] Spec §9 tests validation → Task 9
- [ ] Aucun placeholder `TBD/TODO` — chaque step contient code exact
- [ ] Types cohérents (`McpServerConfig`, `ResolvedMcpServer`, `PublicMcpServerConfig` identiques Tasks 1-9)
- [ ] Secrets jamais dans `RunRequest`/journaux/RPC — vérifié Tasks 3,7,9
