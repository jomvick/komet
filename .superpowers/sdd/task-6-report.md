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
