# Permission UI — Clôture du chantier reasoning + sandbox hardening

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans. Steps use checkbox syntax.

**Goal:** Ce plan conclut l'initiative ouverte par `2026-08-25-reasoning-sandbox-hardening.md` (inspirée de Paseo) sur ses deux volets : le modèle de reasoning ET les niveaux de permission (full-access, etc.). Le hardening précédent a rendu le sandbox strict et validé fail-fast, mais (a) laissé un tuyau non branché (OpenCode), (b) laissé deux trous mineurs dans la validation, et (c) n'a jamais étendu la même rigueur au reasoning — seul le mapping d'id a été fait, pas la validation de compatibilité agent. Ce plan ferme les trois, puis ajoute la couche UI (flux d'approbation interactif ridé sur le doc CRDT existant, comme `InputRequested`, pas de nouveau WS/push — offline-tolerant, multi-device gratuit).

**Dépendance:** `2026-08-25-reasoning-sandbox-hardening.md` mergé (SandboxOptions typé pour `AllowAlways`).

**Dépendance non fermée (vérifiée dans le code, pas supposée) :** `opencode_perms::opencode_config_document` n'est appelé nulle part dans `acp/mod.rs::spawn_agent` — la Task 4c du plan précédent est cochée mais pas branchée. Task 2c ci-dessous ne peut pas fonctionner tant que ce tuyau n'existe pas réellement ; voir Task 2.0.

**Risque intérimaire (à vérifier avant de commencer) :** `claude/mod.rs` bloque déjà `--dangerously-skip-permissions` dès qu'une `sandbox_options.claude` restrictive est présente (hardening précédent, mergé). Sans le `--permission-prompt-tool` de la Task 2a, une session Claude avec une table restrictive tombe aujourd'hui sur un prompt de permission auquel rien ne répond. Voir Task 2a Step 0.

**Architecture:** `PendingPermission` persisté dans `ChatDocHandle` → `SessionCommandPayload::Permit` → harness route vers `--permission-prompt-tool` (Claude) / `approval_policy on-request` (Codex) / `"*":"ask"` (OpenCode). Réutilise le pipeline `doc_host.rs` `Run`/`Steer`.

## Global Constraints

- **Règle de merge CRDT explicite pour les décisions concurrentes.** Deux appareils hors-ligne peuvent décider différemment pour le même `request_id` avant de resynchroniser. Un merge Loro par défaut (last-write-wins sur le champ) peut faire gagner silencieusement un `Allow` périmé sur un `Deny`, ou l'inverse, sur un flux dont l'exemple de ce doc est `rm -rf dist`. Règle retenue : **`Deny` gagne toujours sur conflit** (une décision écartée reste visible comme conflit résolu, pas comme un remplacement silencieux) ; voir Task 1 Step 0.
- **`Scope::Pattern(String)` n'a pas de syntaxe unique.** Codex, Claude et OpenCode ont chacun leur propre langage de règle (`writable_roots`/règles d'approbation Codex, chaînes de permission Claude, clés glob du map `bash` OpenCode). La Task 3 (UI) ne doit générer que des patterns qu'une étape de traduction par provider (Task 2) sait effectivement interpréter — voir Task 0 Step 3 et Task 2.

## Task 0: Proto — events PermissionRequested/Resolved

**Files:** `crates/proto/src/agent.rs`, `crates/proto/src/doc.rs` (ou `crates/engine/src/sessions.rs` selon où vit `AgentEvent`)

- [x] **Step 1: Tests** `permission_requested_round_trips` and `old_wire_without_permission_still_parses`
- [x] **Step 2: Run** `cargo test -p komet-proto`
- [x] **Step 3: Impl** `PermissionKind`, `PermissionChoice`, `Scope`, `AgentPermissionAction`, `PermissionDecision`, `AgentEvent::PermissionRequested/PermissionResolved`
- [x] **Step 4: Run** PASS
- [x] **Step 5: Commit**

## Task 1: Doc — PendingPermission dans ChatDocHandle

**Files:** `crates/doc/src/parts.rs`, `crates/doc/src/schema.rs`, `crates/doc/src/commands.rs`, `crates/engine/src/doc_host.rs`

- [x] **Step 0: Test & Impl — conflit de merge** règle "Deny gagne toujours sur conflit" dans `crates/doc/src/parts.rs`
- [x] **Step 1: Test** `MessagePart::Permission` foldé et résolu dans le doc loro
- [x] **Step 2: Run** PASS
- [x] **Step 3: Impl** `MessagePart::Permission`, `SessionCommandPayload::Permit`, dispatch/claim et résolution
- [x] **Step 4: Run** PASS
- [x] **Step 5: Commit**

## Task -3 : brancher le bouton Access Mode existant sur `sandbox_options` (au lieu du `SandboxLevel` coarse mort)

**Files:** `crates/ui/src/composer.rs`, `crates/ui/src/state.rs`, `crates/proto/src/agent.rs`

- [x] **Step 1: Test** `SandboxOptions::from_level` pour `ReadOnly`, `WorkspaceWrite`, `DangerFullAccess`
- [x] **Step 2: Run** PASS
- [x] **Step 3: Impl** brancher `state.access_mode` et `SandboxOptions::from_level` sur `RunRequest` à l'envoi
- [x] **Step 4: Run** PASS
- [x] **Step 5: Vérification**
- [x] **Step 6: Commit**

## Task -2 : Engine — valider le reasoning level contre l'agent (symétrique au sandbox)

**Files:** `crates/proto/src/agent.rs`, `crates/engine/src/sessions.rs`

- [x] **Step 1: Test** `validation_rejects_unsupported_reasoning_level`, `validation_accepts_reasoning_level_in_agent_ladder`
- [x] **Step 2: Run** PASS
- [x] **Step 3: Impl** `ValidationError::ReasoningLevelUnsupported` et `validate_reasoning`
- [x] **Step 4: Run** PASS
- [x] **Step 5: Commit**

## Task -1 : fermer les deux trous mineurs de `validate_run_request` + 3 corrections vérifiées contre le vrai dépôt Paseo

**Files:** `crates/proto/src/agent.rs`

- [x] **Step 1: Test** `validation_rejects_opencode_patterns_without_wildcard_fallback`, `opencode_fallback_is_keyed_not_positional`, `claude_sandbox_separates_read_and_write_lists`
- [x] **Step 2: Run** PASS
- [x] **Step 3: Impl** `BashPerms::fallback()` par recherche de la clé `"*"`, `ClaudeSandbox.filesystem` étendu à `{ allow_read, deny_read, allow_write, deny_write }`
- [x] **Step 4: Décision**
- [x] **Step 5: Run** `cargo test -p komet-proto` PASS
- [x] **Step 6: Commit**

## Task 2.0 (prérequis bloquant) : fermer le trou OpenCode du hardening précédent

**Files:** `crates/harness/src/acp/mod.rs`, `crates/harness/src/acp/opencode_perms.rs`

- [x] **Step 1: Test** `preserve_pattern_order_and_last_match_semantics`, `document_wraps_permission_section`
- [x] **Step 2: Run** PASS
- [x] **Step 3: Impl** `opencode_config_document`, validation autorisant les configurations appliquées
- [x] **Step 4: Run** PASS
- [x] **Step 5: Commit**

## Task 2: Engine — brancher tuyaux existants

**Files:** `crates/engine/src/sessions.rs`, `crates/harness/src/lib.rs`, `crates/harness/src/acp/mod.rs`, `crates/harness/src/claude/mod.rs`, `crates/harness/src/codex/mod.rs`

- [x] **Step 1: Test** bridge `RunControls.request_permission` & `sessions.respond_permission`
- [x] **Step 2: Run** PASS
- [x] **Step 3: Impl** plumbing de `RunControls` et `doc_host` pour relayer les approbations/refus
- [x] **Step 4: Run** `cargo test -p komet-engine -p komet-harness` PASS
- [x] **Step 5: Commit**

## Task 3: gpui — panneau composer, pas une carte de transcript

**Files:** `crates/ui/src/composer.rs`

- [x] **Step 1: Test** `pending_permission_detection` couvrant les 4 cas de régression
- [x] **Step 2: Run** PASS
- [x] **Step 3: Impl** panneau gpui `render_permission_panel` remplaçant le composer, optimistically masqué au clic, dispatching `SessionCommandPayload::Permit`
- [x] **Step 4: Run** PASS
- [x] **Step 5: Commit**

## Task 4: Doc & garde-fou

- [x] Vieux client sans `PermissionRequested` → compatibilité garantie par serde defaults
- [x] Section docs: approbation = état du docloro (`MessagePart::Permission`), persistant et offline-tolerant
- [x] Section docs: règle de merge CRDT "Deny gagne sur conflit" appliquée au fold dans `crates/doc/src/parts.rs`
- [x] Clôture du chantier permission UI et sandbox/reasoning hardening.
