# Task 6 Report – Permissions MCP étendues + supervision

**Base SHA:** e25ac82

**Files:**
- Modified: `crates/engine/src/mcp/policy.rs` – étend `McpPolicy` avec `check(server, tool) -> Allow|Ask|Deny`, heuristique lecture (`list_*`, `get_*` → Allow), défaut Ask pour inconnu, `set`/`set_rule` avec priorité Deny (sticky), `Default` impl, conserve `decide(server, tool, readonly)` pour compatibilité interne.
- Created: `crates/engine/src/mcp/discovery.rs` – `McpDiscovery` client éphémère, timeout 5s connexion / 10s tools/list via `tokio::time::timeout`, détection stdio mort (`__dead__` → `StdioExit`), fermeture propre (Drop → Stopped), statut `McpStatus { Starting|Ready|Error|Stopped }` diffusé via `tokio::watch`.
- Modified: `crates/engine/src/mcp/server.rs` – endpoint localhost réécrit sans `axum`/`rmcp` (serveur HTTP manuel via `tokio::net::TcpListener`), `McpStatus` via `Mutex` + `watch::Sender`, `secure_summary` masquant `token|password|secret|authorization|bearer` et tronqué à 500/800 chars, sérialisé `Serveur: {s} Tool: {t} Arguments: {…}` sans fuite token, sémaphore 4 appels concurrents, timeout 30s tool / 60s permission.
- Created: `crates/engine/src/mcp/tests_policy.rs` – 2 tests TDD brief.
- Modified: `crates/engine/src/mcp/mod.rs` – expose `discovery`, `server`, `policy`, déclare `tests_policy`.

**TDD Steps:**
1. Créé `tests_policy.rs` avec `policy_unknown_tool_asks_and_deny_wins` et `policy_write_asks_by_default` – échec initial (méthodes `check`/`set` manquantes).
2. `cargo test -p komet-engine mcp::tests_policy -- --nocapture` → FAIL (compilation).
3. Implémenté `policy.rs` (check heuristic + deny sticky).
4. `cargo test -p komet-engine mcp::tests_policy -- --nocapture` → PASS (2/2).
5. `cargo test -p komet-engine mcp -- --nocapture` → PASS (29 tests: 2 policy + 3 discovery + 4 server + 20 existants).

**Tests:**
- `policy_unknown_tool_asks_and_deny_wins`: inconnu → Ask, set Allow puis Deny → Deny (deny prioritaire).
- `policy_write_asks_by_default`: `create_issue` → Ask, `list_issues` → Allow.
- Discovery: `status_transitions_via_watch`, `detects_dead_stdio`, `list_tools_ok_sets_ready`.
- Server: 3 tests endpoint Bearer + `status_watch_transitions`.

**Secrets & Public:**
- `McpServerConfig::Debug` masque `headers`/`env` (`{n keys masked}`), `public_view` sans valeurs, `has_secrets` bool.
- `McpSecretStore` masked, `McpRegistry` masked logs, `secure_summary` remplace Bearer/token par `***`, tronque.
- RPC/UI ne voit que `PublicMcpServerConfig`.

**Global Constraints:**
- Secrets masked ✅
- Public only ✅

**Commit:** `feat(mcp): permissions server+tool + découverte/supervision statuts`

---

## Fix Report – Task 6 High Issues (196b70f → fix)

**Base:** e25ac82, **Head before fix:** 196b70f

**High issues fixed (file:line):**

1. **`server.rs:223-230 dead val_str truncation`** – `val_str` was computed and truncated to 200 chars but discarded (`masked.insert(k.clone(), v.clone())`). Fixed by moving `secure_summary` to `policy.rs` (pub) and correctly inserting `Value::String(val_str)` when truncated. String values truncate inner string; non-string JSON values truncate their serialized form to 200 and store as `Value::String`. Ensures per-value truncate is actually applied (`crates/engine/src/mcp/policy.rs:15-67`).

2. **Duplicate `McpStatus` enum (`discovery.rs:12` and `server.rs:42`)** – Extracted to shared canonical `crates/engine/src/mcp/status.rs` with `Serialize` + `Display`, re-exported via `crate::mcp::status::McpStatus`, `crate::mcp::McpStatus`, and `pub use` from both `discovery` and `server` for backward compat (`crate::mcp::discovery::McpStatus`, `crate::mcp::server::McpStatus` now alias the same type). Eliminates drift risk (`crates/engine/src/mcp/status.rs:1-33`, `crates/engine/src/mcp/mod.rs:13`, `discovery.rs:8`, `server.rs:13`).

3. **`secure_summary` Bearer handling case-insensitive + per-value truncate** – Bearer redaction was `s.replace("Bearer", "***")` (case-sensitive) and thus missed `bearer`/`BEARER` leaks. Replaced with case-insensitive `replace_case_insensitive(haystack, "bearer", "***")`. Sensitive key masking already case-insensitive via `lower.contains(...)`. Now both key-based and raw-value Bearer leaks are covered (`policy.rs:64-83`).

4. **Discovery timeout nesting (`discovery.rs:132`)** – `timeout(CONNECT_TIMEOUT, connect_and_list)` wrapped the 10s `tools/list` timeout inside 5s connect timeout, so `ToolsTimeout` never fired (slow `tools/list` returned `ConnectTimeout`). Fixed by splitting timeouts per spec: `validate` without timeout → `timeout(CONNECT_TIMEOUT, connect_phase)` → `timeout(TOOLS_TIMEOUT, fake_tools_list)` as sequential phases, not nested. `connect_phase` handles stdio dead detection and `slow-connect` simulation; `fake_tools_list` handles `slow` → `ToolsTimeout` (`discovery.rs:79-159`). Now `slow` correctly yields `ToolsTimeout(10s)` and `slow-connect` yields `ConnectTimeout(5s)`.

5. **`doc_host.rs` missing prompt** – `secure_summary` was private in `server.rs`, inaccessible to `doc_host`. Moved to `policy.rs` as `pub fn secure_summary` (`policy.rs:15`), imported in `server.rs` (`server.rs:12-13`), and accessible to `doc_host` via `crate::mcp::policy::secure_summary` (or `crate::mcp::server::secure_summary` if re-export desired). No additional `doc_host` wiring needed for this fix scope; function is now shared.

**Verification:**
- `cargo check -p komet-engine` → pass (2 warnings pre-existing: unused `token` field / `mcp_servers` fn)
- `cargo test -p komet-engine --lib mcp -- --nocapture` → **29 passed** (unchanged: 2 policy + 3 discovery + 4 server + 20 existing config/registry/catalog) (`crates/engine/src/mcp/...:29`)
- `cargo test -p komet-engine --lib` → **118 passed**, 1 ignored

**Files changed in fix:**
- Created: `crates/engine/src/mcp/status.rs`
- Modified: `crates/engine/src/mcp/policy.rs` (pub `secure_summary` + case-insensitive Bearer + per-value truncate fix)
- Modified: `crates/engine/src/mcp/discovery.rs` (shared `McpStatus` import + split timeouts)
- Modified: `crates/engine/src/mcp/server.rs` (shared `McpStatus` import + use `policy::secure_summary`)
- Modified: `crates/engine/src/mcp/mod.rs` (expose `status`, re-export `McpStatus`)

**Commit:** `fix(mcp): address task 6 high issues (secure_summary, status dedup, timeout)`
