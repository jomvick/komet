# Task 5 Report — Injection Claude + matrice capacités

## Status: DONE

## What was implemented

Task 5 per plan `2026-09-05-mcp-externe.md` (commit `feat(mcp): injection Claude + matrice capacités provider`, base `13fc4e0`, 4 files):

1. `crates/harness/src/capabilities.rs` (new, 22 lines) — matrice provider → supports dynamic MCP:
   - `pub fn supports_dynamic_mcp(id: HarnessId) -> (bool, &'static str)` returns `(true, "")` for `ClaudeCode` | `Opencode`, `(false, "Ce provider ne supporte pas l'injection MCP dynamique")` for `Codex`, `Cursor`, `Grok`, `Hermes`, `Pi`, `Antigravity`, `Mock`. Used by UI to disable run / show banner.
   - `pub fn supports_dynamic_mcp_bool(id: HarnessId) -> bool` convenience wrapper (`.0`).
   - No secrets, pure match, `'static` reason for UI.

2. `crates/harness/src/claude/mod.rs` (`build_command` 270-295 → 245-380, +164 lines):
   - Merges `request.mcp` (internal Komet HTTP Bearer) + `request.mcp_external: Vec<ResolvedMcpServer>` into `serde_json::Map<String, Value>` `servers`.
   - Transport mapping:
     - `Stdio` → `{"command": ..., "args": [...], "env": {...}}` (resolved_env if non-empty else config.env)
     - `Http` → `{"type":"http","url":..., "headers":{...}, "env":{...}}`
     - `Sse` → `{"type":"sse","url":..., "headers":{...}, "env":{...}}`
     - Internal `mcp` → `{"type":"http","url":..., "headers":{"Authorization":"Bearer <token>"}}` keyed by `server_name`.
   - Deduplication:
     - By `id`: `servers.contains_key(&id)` → if `existing_url == new_url` → skip (duplicate); else if `!always_load` → skip overwrite (preserve first server, do not clobber user file without `always_load`); if `always_load` true → allow overwrite.
     - By `url`: if `ext.config.url` Some and any existing value has same `url` string → skip (suppress duplicate URL across different ids).
   - If `servers` non-empty → `serde_json::json!({"mcpServers": servers}).to_string()` → `cmd.args(["--mcp-config", &config])`.
   - Logs masked: `tracing::debug!(server_id=%id, "claude mcp dedup ... (masked)")` and `tracing::debug!(server_count=servers.len(), "claude mcpServers injected (values masked)")` — never dumps header/env values.
   - Secrets only at launch via `mcp_external` (in-memory `ResolvedMcpServer`), never via `RunRequest.mcp` transcript.

3. `crates/harness/src/lib.rs` (+1): `pub mod capabilities;`

4. `crates/harness/src/claude/tests_mcp.rs` (new, 100 lines) — TDD tests:
   - `claude_mcp_config_merges_without_duplicate` — `mcp` komet + gh (http) + notion (sse) → `mcpServers.len()==3`
   - `capability_matrix_reports_codex_status` — `(ClaudeCode => (true,""), Codex => (false, msg))`
   - `claude_mcp_dedup_by_id_and_url` — duplicate gh twice → still 3 (id dedup)
   - `claude_mcp_dedup_by_url_suppresses_duplicate_url` — gh + gh2 same url different id → 2 (url dedup)

   Plus internal handling via `pub(crate) fn build_command` visibility change (was private) for test access, and `mod tests_mcp` gated `#[cfg(test)]`.

Global constraints respected:
- Secrets only at launch: `ResolvedMcpServer { resolved_headers, resolved_env }` consumed only inside `build_command`; `cmd` args contain resolved values transiently, never persisted; `tracing::debug!` masks values (only `server_id`/`server_count`).
- Masquer logs: all `tracing::debug!` with `server_id=%id` or `server_count`, no dump of `headers`/`env` values; `ResolvedMcpServer` Debug masks (`{N keys masked}`) preserved from proto.
- `always_load` handling implemented per spec (no overwrite without it); dedup by id and url per spec.

## What was tested and test results

### RED — stub before impl

Created `capabilities.rs` stub returning `(false, "")` for Claude/Opencode (fails), and `tests_mcp.rs` with 4 tests, `build_command` not yet injecting `--mcp-config`:

```
cargo test -p komet-harness claude::tests_mcp -- --nocapture
# -> 4 FAILED
# -> capability_matrix_reports_codex_status: left (false,"") right (true,"")
# -> claude_mcp_config_merges_without_duplicate: --mcp-config present (panic)
# -> claude_mcp_dedup_by_id_and_url: --mcp-config present
# -> claude_mcp_dedup_by_url_suppresses_duplicate_url: --mcp-config present
```

### GREEN — after impl

Fixed `capabilities.rs` to true/false matrix + `claude/mod.rs` fusion logic:

```
cargo test -p komet-harness claude::tests_mcp -- --nocapture
# -> 4 passed (0 failed)
# -> test claude::tests_mcp::capability_matrix_reports_codex_status ... ok
# -> test claude::tests_mcp::claude_mcp_config_merges_without_duplicate ... ok
# -> test claude::tests_mcp::claude_mcp_dedup_by_id_and_url ... ok
# -> test claude::tests_mcp::claude_mcp_dedup_by_url_suppresses_duplicate_url ... ok
```

Full harness:

```
cargo test -p komet-harness -- --nocapture
# -> 119 passed; 2 failed (pre-existing acp model tests, unrelated to Task 5 — also fail on base 13fc4e0 via stash)
# -> acp::tests::models_fall_back_to_legacy_state_with_catalog_options FAILED
# -> acp::tests::models_prefer_the_model_config_option_over_legacy_available_models FAILED
# -> lib pass rate for Task 5-relevant: claude::tests (11) + claude::tests_mcp (4) + capabilities (via tests_mcp) all ok
```

`cargo test -p komet-harness claude::tests_mcp` passes per brief Step 4; `cargo test -p komet-harness -- --nocapture` shows only pre-existing failures, Task 5 slice 100% green.

`cargo check -p komet-harness` → ok (1 warning pre-existing: `mcp_servers` unused).

## Files changed (committed)

- `crates/harness/src/capabilities.rs` (new)
- `crates/harness/src/claude/mod.rs` (build_command fusion + pub(crate) visibility, dedup, always_load, masked logs)
- `crates/harness/src/claude/tests_mcp.rs` (new)
- `crates/harness/src/lib.rs` (mod capabilities)

Commit: `feat(mcp): injection Claude + matrice capacités provider` (base `13fc4e0`).

## Self-review findings

- Completeness: all brief Interfaces implemented: `supports_dynamic_mcp(HarnessId) -> (bool, &'static str)` matrix (true for ClaudeCode/Opencode, false + French message for others), `ClaudeHarness::build_command` fusion via `Map<String,Value>` + `--mcp-config` JSON, dedup by id + url identical suppression, `always_load` guard, secrets only at launch, logs masked. Test file `claude/tests_mcp.rs` matches spec's two required tests (merged without duplicate len 3 + capability matrix) plus two dedup edge cases for url. Brief checklist steps 1-5 done (failing → verified FAIL → minimal impl → verified PASS → commit).
- Quality: transport-specific JSON per Claude's `mcpServers` shape (stdio command/args/env, http/sse type/url/headers/env), internal komet injected first, dedup checks both id and cross-url, always_load respects spec "n'écrase pas fichier utilisateur sans always_load", `pub(crate)` visibility minimal, `#[cfg(test)] mod tests_mcp` no prod bloat, `tracing::debug!` masked, no `println!` secrets.
- Discipline / YAGNI: no file I/O for Claude (inline `--mcp-config` JSON per spec snippet, not tempfile per internal design decision table); no UI wiring yet (Task 8); no Codex/Cursor injection (matrix returns false, correct per spec "à confirmer" → false). No proto/engine changes (Task 5 is harness-only).
- Tests: real behavior via `tokio::process::Command` argv inspection, `extract_mcp_config` parses `--mcp-config` JSON, asserts `mcpServers` object len 3 / dedup len 3 / url dedup len 2. Capability test asserts tuple equality per spec's French message verbatim. TDD evidence RED then GREEN recorded. No secrets in repo grep (`gh-secret`, `Bearer` only in test `resolved_*` in-memory, not persisted).
- Issues fixed before report: stub → full impl; private `build_command` → `pub(crate)` for `tests_mcp` visibility; `capabilities.rs` stub false → true/false matrix; dedup logic added to pass duplicate tests; `lib.rs` mod added; verified `cargo test -p komet-harness claude::tests_mcp` PASS.

## Issues/concerns

- Pre-existing failures `acp::tests::models_*` (2) fail on base `13fc4e0` (verified via `git stash` → same FAIL). Not introduced by Task 5; they assert `serviceTier`/`Ultra` options from `codex::catalog::static_models()` which currently lack those options. Should be fixed in catalog or test, but out of scope for Task 5; harness suite for Task 5 slice is green. `cargo test -p komet-harness claude::tests_mcp -- --nocapture` is the brief's gating check and passes.
- Spec discrepancy: brief Step 1 snippet `supports_dynamic_mcp(HarnessId::ClaudeCode) == true` expects `bool`, but Step 3 defines `-> (bool, &'static str)` tuple. Implemented tuple (spec Step 3 authoritative for UI reason), test adapted to assert `(true, "")` / `(false, "...")`. Brief's bool snippet would not compile against tuple; report documents gap. Provided `supports_dynamic_mcp_bool` helper for call sites needing bool-only.
- `--mcp-config` inline JSON vs tempfile: brief snippet uses `cmd.args(["--mcp-config", &config])` with `json!({"mcpServers": servers}).to_string()` inline. Implemented per snippet (inline). If Claude CLI requires file path (per `2026-09-05-mcp-interne-komet` decision table branch (a)), future Task may switch to `tempfile` with file path arg; current inline satisfies spec and tests.
- `always_load` semantics: spec "n'écrase pas fichier utilisateur sans `always_load`" interpreted as "do not overwrite an existing `id` entry already in `servers` Map without `always_load`". Since Claude injection uses `--mcp-config` (not file), there is no user file to overwrite, but dedup-by-id with `always_load` guard future-proofs. If Task 6+ adds file merging, guard will apply.
