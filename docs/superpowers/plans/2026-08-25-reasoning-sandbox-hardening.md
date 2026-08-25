# Reasoning + Sandbox Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Renforcer le modèle komet en gardant l'enum unifiée pour reasoning mais en ajoutant validation stricte + options granulaires type Paseo pour le sandbox.

**Architecture:** Garder `ReasoningLevel`/`SandboxLevel` comme enums globales (compat), ajouter `SandboxOptions` provider-natif validé (serde strict) + `ReasoningLevel::from_thinking_option_id` mapping. `RunRequest` porte `sandbox_options`. Engine valide avant spawn et fail-fast.

**Tech Stack:** Rust, serde(deny_unknown_fields), komet-proto, komet-harness, komet-engine

## Global Constraints
- Enum existante non breakée (serde compat) — `RunRequest.sandbox: SandboxLevel` reste, `sandbox_options` est additif `#[serde(default, skip_serializing_if="Option::is_none")]` comme `agent.rs:97-123`
- Validation fail-fast avant spawn, erreur structurée exploitable (pas `String` opaque)
- Pas de host boundary magique : doc que sandbox = contrainte CLI, pas container — section sécurité visible README/security.md (comme Paseo), pas seulement commentaire
- Règle de précédence explicite (Paseo `modeId`/`options` — *options wins*): si `sandbox_options` est `Some`, il prime sur `sandbox: SandboxLevel`; sinon fallback `SandboxLevel`. `auto_approve` ne doit PAS écraser silencieusement `sandbox_options` explicite (verdict §2)

## Contexte Code Réel (verdict — à respecter)
- `RunRequest.model_options: Map<String, Value>` existe déjà `agent.rs:101-103` (ex: `serviceTier` chez Codex `codex/mod.rs:514`). Ne pas dupliquer silencieusement : `sandbox_options` est un champ typé séparé, pas un sous-espace de `model_options`.
- `codex/mod.rs:330` force `request.sandbox = DangerFullAccess` en yolo + `codex/mod.rs:510` `approval_policy="never"` hardcodé; `claude/mod.rs:212` `--dangerously-skip-permissions` en `auto_approve`. Sans garde, `sandbox_options` granulaire est écrasé exactement où il est le plus critique.
- OpenCode n'a aucune gestion de permission `acp/subagent_opencode.rs` — travail neuf; schéma le plus subtil (map patterns bash où dernière clé = fallback `"*":"ask"` + actions sans cible `webfetch`/`todowrite`...)

---

### Task 0: Proto — Régression compat wire (pré-requis)

**Files:**
- Test: `crates/proto/src/agent.rs` (tests module)

**Objectif:** Garantir que l'ajout `sandbox_options` ne casse pas les vieux clients (promesse compat du plan).

- [x] **Step 1: Write failing test** (doit passer dès maintenant — garde-fou)
```rust
#[test]
fn run_request_old_wire_without_sandbox_options_still_parses() {
    let old = r#"{"prompt":"p","cwd":".","sandbox":"workspace-write"}"#;
    let req: RunRequest = serde_json::from_str(old).unwrap();
    assert!(req.sandbox_options.is_none());
    assert_eq!(req.sandbox, SandboxLevel::WorkspaceWrite);
    // round-trip: None serialise away (old readers never see it)
    let json = serde_json::to_value(&req).unwrap();
    assert!(json.get("sandboxOptions").is_none());
}
```
- [x] **Step 2: Run test** `cargo test -p komet-proto run_request_old_wire -- --nocapture` Expected PASS (sinon fix `#[serde(default, skip_serializing_if="Option::is_none")]`)
- [x] **Step 3: Commit** si ajustement serde nécessaire

### Task 1: Proto — SandboxOptions provider-natif (tableau Paseo complet)

**Files:**
- Modify: `crates/proto/src/agent.rs:41-47`
- Test: `crates/proto/src/agent.rs` (tests module)
- Ref: `paseo.sh/docs/sdk/provider-options` (tableau complet, pas sous-ensemble)

**Interfaces:**
- Produces: `SandboxOptions { codex: Option<CodexSandbox>, claude: Option<ClaudeSandbox>, opencode: Option<OpenCodePerms> }` + `RunRequest.sandbox_options: Option<SandboxOptions>` avec `#[serde(default, skip_serializing_if="Option::is_none")]`

**Schéma à reprendre tel quel (verdict § Recommandations):**
- `CodexSandbox`: `sandbox_mode`, `writable_roots: Vec<PathBuf>`, `network_access: bool`, `web_search: bool`, `features: Vec<String>`, `approval_policy: ApprovalPolicy` où `ApprovalPolicy::Never` | `Granular { ask, auto_approve, ... }` — reflète Paseo `approval_policy: "never" only removes prompts, sandbox_mode controls access`
- `ClaudeSandbox`: `filesystem: FilesystemSandbox { allow, deny }`, `network: NetworkSandbox`, `allow_unsandboxed_commands: bool`, `excluded_commands: Vec<String>`, `fail_if_unavailable: bool`, `settings_permissions: Value` — reflète Paseo `settings.permissions`
- `OpenCodePerms`: `BashPerms { patterns: IndexMap<String, Perm> }` où **dernière clé = fallback** (`"*": "ask"` sémantique Paseo) + `unscoped_actions: Map<String, Perm>` pour `webfetch`/`websearch`/`todowrite`/etc.

- [x] **Step 1: Write failing tests**
```rust
#[test]
fn sandbox_options_rejects_unknown_field() {
    let json = r#"{"sandboxMode":"workspace-write","unknown":1}"#;
    assert!(serde_json::from_str::<CodexSandbox>(json).is_err());
}
#[test]
fn opencode_perms_last_key_is_fallback() {
    let json = r#"{"bash":{"*":"ask","git status":"allow"}}"#;
    let perms: OpenCodePerms = serde_json::from_str(json).unwrap();
    assert_eq!(perms.bash_fallback(), Some(Perm::Ask));
}
```
- [x] **Step 2: Run tests** `cargo test -p komet-proto sandbox_options -- --nocapture` Expected FAIL
- [x] **Step 3: Implement** structs avec `#[serde(deny_unknown_fields)]`, `#[serde(rename_all="camelCase")]`, enums `SandboxMode { ReadOnly, WorkspaceWrite, DangerFullAccess }`, `Perm { Allow, Ask, Deny }`
- [x] **Step 4: Run tests** Expected PASS + `cargo test -p komet-proto -- --nocapture` green
- [x] **Step 5: Commit** `git add crates/proto/src/agent.rs && git commit -m "feat(proto): strict SandboxOptions (Paseo-complete)"`

### Task 2: Proto — thinkingOptionId mapping

**Files:**
- Modify: `crates/proto/src/agent.rs:25-39`
- Modify: `crates/harness/src/lib.rs` (Harness::reasoning_levels doc)

**Interfaces:**
- Produces: `ReasoningLevel::from_thinking_id(&str) -> Option<Self>` + `as_thinking_id(&self) -> &str`

- [x] **Step 1: Test** `assert_eq!(ReasoningLevel::from_thinking_id("xhigh"), Some(XHigh))`
- [x] **Step 2: Run** FAIL
- [x] **Step 3: Impl** match table minimal/low/medium/high/xhigh/max/ultra/ultracode/ultrathink
- [x] **Step 4: Run** PASS
- [x] **Step 5: Commit**

### Task 3: Engine — validation fail-fast (généralisée + structurée)

**Files:**
- Modify: `crates/engine/src/doc_host.rs:2204` (run creation)
- Modify: `crates/proto/src/agent.rs` (ajout `ValidationError` enum)
- Test: `crates/engine/tests/` + `crates/proto/tests`

**Interfaces:**
- Consumes: `RunRequest { sandbox, sandbox_options, auto_approve, cwd }`
- Produces: `validate_run_request(&RunRequest) -> Result<(), ValidationError>` appelé avant spawn; `ValidationError` enum (pas `String`) pour UI/testabilité
- Règle de précédence (à tester): `if sandbox_options.is_some() { use sandbox_options } else { fallback SandboxLevel }` + **garde yolo**: `if sandbox_options.is_some() { ne pas forcer DangerFullAccess }` — corrige `codex/mod.rs:330` sinon Task 4 inopérante en `auto_approve`

**Validation à généraliser aux 3 providers (pas seulement `writable_roots`):**
- Codex: `writable_roots` hors `cwd` sans `DangerFullAccess`, `network_access=true` sans `DangerFullAccess`, `approval_policy Never` sans `sandbox_mode`
- Claude: `filesystem.allow` hors `cwd`, `allow_unsandboxed_commands` avec `fail_if_unavailable`
- OpenCode: pattern map vide sans fallback `"*"`, perm inconnue

- [x] **Step 1: Tests** (3 providers + yolo)
```rust
#[test]
fn validation_rejects_writable_root_outside_cwd() { /* ... -> ValidationError::WritableRootOutsideCwd */ }
#[test]
fn validation_sandbox_options_wins_over_sandbox_level() { /* sandbox=ReadOnly + options=Danger -> options wins */ }
#[test]
fn validation_yolo_does_not_override_explicit_options() { /* auto_approve=true + sandbox_options=ReadOnly -> reste ReadOnly */ }
```
- [x] **Step 2: Run** FAIL
- [x] **Step 3: Impl** `ValidationError` enum + `validate_run_request` + early return `Done { status: Errored, error: Some(validation.to_string()) }`, log `tracing::warn!(?validation)`
- [x] **Step 4: Run** PASS (`cargo test -p komet-engine -- --nocapture`)
- [x] **Step 5: Commit**

### Task 4: Harness — traduction options → flags natifs (par provider)

**Files:**
- Modify: `crates/harness/src/codex/mod.rs` (dont `codex/mod.rs:330` garde yolo + `codex/mod.rs:510` approval_policy), `crates/harness/src/claude/mod.rs:171` (`build_command`), `crates/harness/src/acp/subagent_opencode.rs` (ou `crates/harness/src/acp/mod.rs` — nouveau)
- Test: `crates/harness/tests/` (3 providers)

**4a — Codex:**
- [x] **Step 1: Test** `CodexHarness` avec `SandboxOptions { codex: Some(CodexSandbox { sandbox_mode: ReadOnly, .. }) }` → `turn/start` params contiennent `"sandbox":"read-only"` + `sandboxPolicy` correct + `approvalPolicy` respecte `Granular` quand présent (pas toujours `"never"`)
- [x] **Step 2: Run** FAIL
- [x] **Step 3: Impl** mapping `SandboxOptions.codex` → `sandbox_mode`/`sandboxPolicy`/`approvalPolicy` (ref `paseo.sh/docs/sdk/provider-options` + `public-docs/sdk/provider-options.md:32,76`), conditionner `request.sandbox = DangerFullAccess` seulement si `sandbox_options.is_none()`

**4b — Claude:**
- [x] **Step 1: Test** `ClaudeHarness` avec `ClaudeSandbox { filesystem, network, allow_unsandboxed_commands: false }` → args contiennent `--permission-mode default` + settings JSON avec `permissions` restreintes (pas `--dangerously-skip-permissions` quand sandbox restreint)
- [x] **Step 2: Run** FAIL
- [x] **Step 3: Impl** mapping `SandboxOptions.claude` → CLI flags/settings (`claude/mod.rs:212` à conditionner: `dangerously-skip-permissions` seulement si `sandbox_options` est `None` ou `DangerFullAccess`)

**4c — OpenCode (nouveau, pas traduction):**
- [x] **Step 1: Test** `OpenCodePerms` avec `{"bash":{"*":"ask","git *":"allow"}}` → config générée respecte fallback dernière-clé + actions sans cible
- [x] **Step 2: Run** FAIL
- [x] **Step 3: Impl** génération `opencode.json`/`settings` ou `session/set_config_option` via ACP (selon `acp/mod.rs`), avec préservation de l'ordre `IndexMap`

- [x] **Step 4: Run** `cargo test -p komet-harness -- --nocapture` PASS
- [x] **Step 5: Commit** `git add crates/harness && git commit -m "feat(harness): map SandboxOptions to native flags (codex/claude/opencode)"`

### Task 5: Doc & garde-fou final

**Files:**
- Modify: `README.md` ou `docs/security.md` (nouveau), `docs/superpowers/plans/2026-08-25-reasoning-sandbox-hardening.md` (self-review)

- [x] **Step 1: Test** vérif manuelle: lancer avec vieux client `sandbox` seul → comportement identique à aujourd'hui (Task 0)
- [x] **Step 2: Doc** section "Sandbox is not a host boundary" visible (comme Paseo) — CLI constraint, pas container/isolation host
- [x] **Step 3: Commit** `docs: sandbox security note`

**Self-Review (mis à jour):** couvre reasoning unifié + sandbox granulaire Paseo-complet + validation structurée + précédence `options wins` + garde yolo + OpenCode neuf + compat wire + doc sécurité. Pas de placeholder. Types cohérents.

---
Plan complete and saved to `docs/superpowers/plans/2026-08-25-reasoning-sandbox-hardening.md`. Two execution options:

**1. Subagent-Driven (recommended)** - I dispatch a fresh subagent per task, review between tasks, fast iteration
**2. Inline Execution** - Execute tasks in this session using executing-plans, batch execution with checkpoints

Which approach?
