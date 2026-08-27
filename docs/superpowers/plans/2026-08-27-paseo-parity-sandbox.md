# Paseo Parity Sandbox Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Aligner le modèle sandbox/permissions komet sur Paseo (provider-native strict + options wins + ask→permission) pour Codex/Claude/OpenCode, et fermer la porte aux autres providers.

**Architecture:** Étendre `SandboxOptions` aux champs Paseo manquants, durcir `validate_run_request` (fail-fast structuré), brancher OpenCode via overlay `OPENCODE_CONFIG`, et rejeter `options` non-vides pour Cursor/Grok/Hermes/Pi/Antigravity. UI `PermissionRequested` déjà en place, on la rend bloquante via `request_permission` bridge.

**Tech Stack:** Rust, serde(deny_unknown_fields), komet-proto, komet-harness, komet-engine, TS client Paseo ref `public-docs/sdk/provider-options.md`

## Global Constraints

- `RunRequest.sandbox` reste compat wire `#[serde(default,skip_serializing_if)]` — `sandbox_options` additif.
- Validation structurée `ValidationError` enum, pas string opaque, avant spawn.
- `options wins` sur `modeId`/`sandbox` legacy, yolo n'écrase pas `sandbox_options` explicite.
- Pas de host boundary — doc security.md, pas isolation OS.

---

### Task 0: Proto — compléter schémas Paseo manquants

**Files:**
- Modify: `crates/proto/src/agent.rs:173-350` (CodexSandbox, ClaudeSandbox, OpenCodePerms)
- Test: `crates/proto/src/agent.rs` tests module

**Interfaces:**
- Consumes: Paseo `public-docs/sdk/provider-options.md:51,101,151`
- Produces: `CodexSandbox { sandbox_workspace_write: { exclude_slash_tmp, exclude_tmpdir_env_var } }`, `ClaudeSandbox { allowedTools, disallowedTools, additionalDirectories, settings.sandbox }`, `OpenCodePerms { read, edit, external_directory, ... }`

- [x] **Step 1: Write failing test** `sandbox_options_rejects_unknown_field` + `codex_exclude_tmp` + `claude_allowed_tools` + `opencode_external_directory`
- [x] **Step 2: Run** `cargo test -p komet-proto -- --nocapture` Expected FAIL
- [x] **Step 3: Impl** champs manquants avec `deny_unknown_fields`, `rename_all=camelCase`, defaults.
- [x] **Step 4: Run** Expected PASS
- [x] **Step 5: Commit** `feat(proto): paseo parity fields`

### Task 1: Validation — fail-fast + autres providers rejettent options

**Files:**
- Modify: `crates/proto/src/agent.rs:600` `validate_run_request`
- Test: `crates/engine/tests/sandbox_validation.rs`

**Interfaces:**
- Produces: `ValidationError::ProviderOptionsRejected { provider }` quand `sandbox_options` non-vide pour Cursor/Grok/Hermes/Pi/Antigravity (ref Paseo "every other provider rejects non-empty options").

- [x] **Step 1: Write test** `other_provider_with_options_rejected` — `RunRequest { harness: Some(Cursor), sandbox_options: Some(Codex{...}) } -> Err`
- [x] **Step 2: Run** FAIL
- [x] **Step 3: Impl** check `if harness in [Cursor,Grok,Hermes,Pi,Antigravity] && sandbox_options.is_some_and(|o| !o.is_empty()) => Err`
- [x] **Step 4: Run** PASS
- [x] **Step 5: Commit** `feat(proto): reject options for non-sandbox providers`

### Task 2: Harness — brancher OpenCode overlay

**Files:**
- Modify: `crates/harness/src/acp/mod.rs` spawn_agent
- Modify: `crates/harness/src/acp/opencode_perms.rs` (déjà prêt)
- Test: `crates/harness/tests/acp.rs`

**Interfaces:**
- Consumes: `opencode_config_document(&OpenCodePerms) -> String`
- Produces: écrit overlay temp file, `env OPENCODE_CONFIG=<overlay>` au spawn opencode, merge vérifié ne clobber pas user config.

- [x] **Step 1: Write test** `opencode_overlay_injected` — spawn avec perms Ask -> env contient overlay JSON avec bash "*":ask
- [x] **Step 2: Run** FAIL
- [x] **Step 3: Impl** créer fichier temp `opencode.json` overlay via `opencode_config_document`, set `OPENCODE_CONFIG`, vérifier merge semantics live.
- [x] **Step 4: Run** PASS
- [x] **Step 5: Commit** `feat(harness): wire opencode permission overlay`

### Task 3: Harness — Codex/Claude mapping complet Paseo

**Files:**
- Modify: `crates/harness/src/codex/mod.rs:526`, `crates/harness/src/claude/mod.rs:210`
- Test: `crates/harness/tests/codex.rs`, `crates/harness/tests/clause` (fake)

**Interfaces:**
- Produces: Codex `sandbox_workspace_write.exclude_*` + `web_search` mapping, Claude `allowedTools/disallowedTools` + `sandbox.filesystem/network` complet.

- [x] **Step 1: Write test** `codex_exclude_tmp_mapped` + `claude_allowed_tools_mapped`
- [x] **Step 2: Run** FAIL
- [x] **Step 3: Impl** mapping 1:1 vers wire params / CLI flags
- [x] **Step 4: Run** PASS
- [x] **Step 5: Commit** `feat(harness): complete paseo codex/claude mapping`

### Task 4: Engine — permission bridge bloquant (ask→permission)

**Files:**
- Modify: `crates/engine/src/sessions.rs:708` `respond_permission` (déjà deny→interrupt), `crates/harness/src/acp/mod.rs:1988` `sandbox_requires_ask`
- Test: `crates/engine/tests/permission_flow.rs`

**Interfaces:**
- Consumes: `PermissionChoice::Ask` -> `AgentEvent::PermissionRequested` bloquant, `Deny` -> `interrupt` (déjà fait), `Allow` -> `selected`.
- Produces: `request_permission` await réel avant `client.respond`, pas fire-and-forget demo.

- [x] **Step 1: Write test** `ask_blocks_until_permit` + `deny_stops_agent`
- [x] **Step 2: Run** FAIL
- [x] **Step 3: Impl** `handle_server_request_live` await `request_permission` oneshot, `respond_permission` résout + interrupt si Deny
- [x] **Step 4: Run** PASS
- [x] **Step 5: Commit** `feat(engine): blocking permission bridge`

### Task 5: Doc & garde-fou

**Files:**
- Modify: `docs/security.md:35`, `README.md`

- [x] **Step 1: Update** security.md retirer "REJECTED", documenter OpenCode branché + autres providers rejettent options
- [x] **Step 2: Commit** `docs: paseo parity security note`
