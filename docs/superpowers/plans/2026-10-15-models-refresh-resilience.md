# Modèles : refresh résilient (anti-timeout, cache-first, fallback catalogue) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ne plus jamais afficher `harness protocol error: claude model discovery timed out` — ni bloquer le picker — quand le CLI est sain mais lent à démarrer : un timeout adapté au cold start, une liste connue servie instantanément, et un catalogue curé en dernier recours plutôt qu'une erreur morte.

**Architecture:** Trois couches. (1) *Harness* : chaque sonde live garde son contrat live-only mais avec un budget qui absorbe un cold start (45s) et un mémo in-session plus long (60s) pour qu'un switch d'agent ne re-spawn pas le CLI. (2) *Engine* : `models(force=false)` devient cache-first sans borne d'âge (stale-while-revalidate en arrière-plan au-delà d'1h) ; `force=true` (Rescan/Retry) reste une sonde live stricte ; sans cache et avec une sonde en échec, `Harness::fallback_models()` (catalogue curé) alimente l'affichage pendant que la re-sonde tourne. (3) *UI* : l'ouverture du picker et le changement d'agent ne forcent plus une sonde (revalidation bornée à 60s), et un échec de reload ne blanchit jamais une liste déjà affichée.

**Tech Stack:** Rust (tokio, serde_json), GPUI pickers, harnesses Claude/Codex/Cursor/Antigravity/ACP (grok, hermes, opencode, cline).

**Spec:** Rapport utilisateur du 2026-10-15 sur la branche `feat/transparent-cli-errors` (suite de `docs/superpowers/plans/2026-10-12-models-refresh-provider-sync.md`) : « harness protocol error: claude model discovery timed… » à chaque ouverture/switch, alors que le CLI fonctionne. Le brief live-only (aucun fallback statique servi comme *succès*) reste la règle : le catalogue curé n'est qu'un affichage de secours, jamais une réussite de sonde.

## Global Constraints

- `Harness::models()` reste **live-only** : aucun `Ok(static_models())`. Le catalogue curé passe par le nouveau `Harness::fallback_models()` et n'est **que** de l'affichage.
- Le send-path ne change pas : un id absent du catalogue du CLI reste refusé (`sync_send_model_id`, chip « unavailable »). Un fallback ne peut donc jamais lancer un run mort.
- Rescan / Retry = `force=true` : sonde live, l'échec se propage, jamais masqué par un cache ou un fallback.
- Pas de nouvelle dépendance ; budgets en constantes nommées et commentées (cold start mesuré).
- Montrer le diff complet avant de committer. Pas de push.

## Review Focus

- CLI sain mais lent (> 10s de cold start) → pas d'erreur, pas de spinner infini : la sonde aboutit ou le catalogue curé s'affiche, puis la re-sonde remplace la liste.
- Picker ouvert / agent switché deux fois de suite → une seule sonde par harness (aucun re-probe < 60s), y compris en flip-flop entre deux agents.
- CLI cassé avec un cache existant → la liste connue reste affichée (aucun blanchiment, aucune erreur affichée à la place).
- CLI cassé sans cache, harness à catalogue (claude, codex, antigravity) → catalogue affiché + re-sonde en arrière-plan ; harness sans catalogue (cursor, ACP : grok/hermes/opencode/cline) → l'erreur lisible reste la seule réponse honnête.
- Reload en arrière-plan qui échoue sur une liste affichée → log `warn`, lignes conservées, horodatage inchangé pour que la prochaine ouverture réessaie.
- Rescan concurrent / double-clic → toujours une seule sonde par harness.

---

### Task 1: Budgets de sonde adaptés au cold start

**Files:**
- Modify: `crates/harness/src/claude/mod.rs`
- Modify: `crates/harness/src/codex/mod.rs`
- Modify: `crates/harness/src/cursor/mod.rs`
- Modify: `crates/harness/src/antigravity/mod.rs`
- Modify: `crates/harness/src/acp/mod.rs`
- Test: `crates/harness/tests/claude.rs`, `crates/harness/tests/antigravity.rs`, `crates/harness/tests/acp.rs`

**Interfaces:**
- Consumes: les budgets existants (`Duration::from_secs(10|15|20)`, `DEFAULT_MODEL_DISCOVERY_TIMEOUT`).
- Produces: `MODEL_DISCOVERY_TIMEOUT = 45s` (constant par harness / défaut ACP) ; mémo in-session `models` porté à 60s. Inchangé : `Harness::models() -> Result<Vec<Model>, HarnessError>` et la propagation `NotInstalled` / `Protocol(msg)`.

- [x] **Step 1: Reproduire** — `harness.models()` sur un CLI Claude sain mais froid dépasse 10s → `Protocol("claude model discovery timed out")` (cold start : auth + chargement config/MCP).
- [x] **Step 2: Relever le budget** — Claude 10s → 45s, Codex 15s → 45s, Cursor 20s/15s → 45s, Antigravity 15s → 45s, défaut ACP `DEFAULT_MODEL_DISCOVERY_TIMEOUT` 10s → 45s (opencode/cline gardent leur budget partagé avec le handshake). Commentaire chiffré au-dessus de chaque constante (cold start observé).
- [x] **Step 3: Mémo in-session** — `models` garde la main sur un TTL de 10s → 60s (Claude, Antigravity) : un retour sur un agent ne re-spawn plus le CLI, un `Rescan` au-delà repart en live.
- [x] **Step 4: Vérifier** — `cargo test -p komet-harness` : les tests live-only (échec de sonde → `Err`, pas de catalogue) restent verts.

### Task 2: Catalogue curé comme dernier recours (affichage seulement)

**Files:**
- Modify: `crates/harness/src/lib.rs`
- Modify: `crates/harness/src/claude/mod.rs`
- Modify: `crates/harness/src/codex/mod.rs`
- Modify: `crates/harness/src/antigravity/mod.rs`
- Modify: `crates/harness/src/cursor/mod.rs`
- Test: `crates/engine/src/registry.rs` (mod tests)

**Interfaces:**
- Consumes: `catalog::static_models()` de Claude/Codex/Antigravity.
- Produces: `Harness::fallback_models(&self) -> Vec<Model>` (défaut `Vec::new()`), overridé par Claude, Codex et Antigravity. Cursor ne l'override pas (ses lignes de catalogue sont `#[cfg(test)]`, fixtures et non une liste de production) ; les harnesses ACP non plus (pas de catalogue).
- Produces: `HarnessRegistry::fallback_or_error(id, harness, error)` → catalogue curé + `spawn_background_refresh(id)`, ou l'erreur si le harness n'a pas de catalogue.

- [x] **Step 1: Test qui échoue** — probe cassée sans cache :

```rust
#[tokio::test]
async fn failed_probe_without_cache_serves_the_curated_fallback() {
    let registry = HarnessRegistry::new();
    let harness = komet_harness::claude::ClaudeHarness::new()
        .with_executable("/nonexistent/never-a-claude");
    registry.register(Arc::new(harness) as Arc<dyn Harness>);
    let models = registry.models(HarnessId::ClaudeCode, false).await
        .expect("a curated fallback must outlive a failed first probe");
    assert_eq!(models.len(), komet_harness::claude::catalog::static_models().len());
}
```

- [x] **Step 2: Run** — `cargo test -p komet-engine failed_probe_without_cache` : ÉCHEC avant implémentation (`Err(Protocol)`).
- [x] **Step 3: Implémenter** — `fallback_models()` sur le trait + les trois harnesses à catalogue ; `models()` appelle `fallback_or_error` sur `Ok(empty)` comme sur `Err`.
- [x] **Step 4: Sécurité du send-path** — vérifier qu'un id de catalogue non annoncé par le CLI reste refusé à l'envoi (couvert par `crates/ui/src/pickers.rs`, `sync_send_model_id`).
- [x] **Step 5: Run** — `cargo test -p komet-engine` : vert.

### Task 3: Engine cache-first (plus de sonde bloquante sur une liste connue)

**Files:**
- Modify: `crates/engine/src/registry.rs`
- Modify: `crates/engine/src/rpc.rs`
- Test: `crates/engine/src/registry.rs` (mod tests)

**Interfaces:**
- Consumes: `models_cache` (persistant), `MODELS_CACHE_REFRESH_AFTER` (1h), `spawn_background_refresh`.
- Produces: `HarnessRegistry::models(id, force)` — `force=false` : entrée connue servie immédiatement quelle que soit son âge, revalidation en arrière-plan au-delà d'1h, sinon une sonde + fallback. `force=true` → `probe_live(id)` (aucun masquage).
- Produces: `store_models(id, now, models)`, `fallback_or_error(id, harness, error)` (remplace `stale_or_error`). `MODELS_CACHE_TTL` disparaît (plus de borne d'âge sur le service).

- [x] **Step 1: Test qui échoue** — un hit de cache ne doit pas résoudre le harness :

```rust
// premier load : 1 résolution ; cache remplacé par un marqueur ;
// models(force=false) → marqueur, resolves toujours 1 ;
// models(force=true) → sonde réelle, pas le marqueur.
```

- [x] **Step 2: Run** — `cargo test -p komet-engine cached_models_are_served_without_resolving_the_harness`.
- [x] **Step 3: Implémenter** — cache-first sans borne d'âge ; `force` → `probe_live` ; écriture atomique factorisée dans `store_models`.
- [x] **Step 4: Doc RPC** — commentaire de `methods::LIST_MODELS` aligné : `force` n'est plus envoyé à chaque ouverture, seulement Rescan/Retry.
- [x] **Step 5: Run** — `cargo test -p komet-engine` : vert.

### Task 4: UI — revalidation bornée + stale-while-revalidate

**Files:**
- Modify: `crates/ui/src/pickers.rs`

**Interfaces:**
- Consumes: `ensure_models(harness, force, cx)`, `prefetch_models`, la réouverture du picker, `select_harness`.
- Produces: `enum ModelsLoad { Fill, Revalidate, Force }`, `const MODELS_REVALIDATE_AFTER = 60s`, `Pickers::models_loaded_at: HashMap<HarnessId, Instant>`, `Pickers::models_slot_stale(harness) -> bool`.
- Produces (comportement) : ouverture/switch → `Revalidate` (RPC `force: false`) ; prefetch render-loop → `Fill` ; Rescan/Retry → `Force` (RPC `force: true`). Un `Loadable::Error` de *reload* conserve la liste `Ready` à l'écran et laisse l'horodatage intact.

- [x] **Step 1: Remplacer le force-par-ouverture** — `ensure_models(effective, true)` (ouverture du picker) et `ensure_models(harness, true)` (switch) deviennent `ModelsLoad::Revalidate`.
- [x] **Step 2: Revalidation bornée** — `models_slot_stale` : un slot `Ready` de moins de 60s est réutilisé tel quel (flip-flop entre deux agents = zéro re-probe).
- [x] **Step 3: Stale-while-revalidate** — pas de passage par `Loading` si une liste est déjà affichée ; un reload qui échoue garde la liste + `tracing::warn!`, l'horodatage n'est pas mis à jour pour que la prochaine ouverture réessaie.
- [x] **Step 4: Vérifier** — `cargo test -p komet-ui` : vert.

---

### Task 5: Claude — le sous-type de sonde a changé (`list_models`)

**Files:**
- Modify: `crates/harness/src/claude/mod.rs`
- Modify: `crates/harness/src/claude/catalog.rs`
- Test: `crates/harness/tests/claude.rs`
- Test: `crates/harness/tests/fixtures/fake-claude.sh`, `fake-claude-legacy-models.sh` (nouveau)

**Interfaces:**
- Consumes: le canal de contrôle Claude (`--print --input-format stream-json --output-format stream-json --verbose`).
- Produces: `discover_live_models` sonde `list_models` puis `supported_models` (repli seulement si le CLI rejette explicitement le premier comme « unsupported ») et, si les deux échouent, remonte le message du CLI.
- Produces: `catalog::wire_ladder(item)` — échelle d'effort dérivée de `supportedEffortLevels` ; options dérivées de `supportsFastMode` et du suffixe `[1m]` de `resolvedModel`.
- Produces: `supports_xhigh` reconnaît les alias du CLI (`default`, `opus[1m]`, `sonnet`) par forme exacte, sans casser l'exclusion `opus-4-5` / `sonnet-4-5`.

**Constat (CLI 2.1.218, installé) :** `supported_models` n'existe plus — le CLI répond `{"subtype":"error","error":"Unsupported control request subtype: supported_models"}`, ce qui vidait la liste et produisait `harness protocol error: claude supported_models returned no usable models`. Le sous-type vivant est `list_models`, avec des ids d'alias et les capacités du CLI :

```json
{"value":"opus[1m]","resolvedModel":"claude-opus-4-8[1m]","displayName":"Opus",
 "description":"Opus 4.8 with 1M context …","supportsEffort":true,
 "supportedEffortLevels":["low","medium","high","xhigh","max"],"supportsFastMode":true}
```

- [x] **Step 1: Reproduire live** — `printf '{"type":"control_request","request_id":"p","request":{"subtype":"supported_models"}}\n' | claude --print …` → erreur « Unsupported control request subtype ».
- [x] **Step 2: Sonde à deux sous-types** — `list_models` d'abord, `supported_models` en repli, et l'erreur du CLI remonte telle quelle.
- [x] **Step 3: Parse piloté par le fil** — ids/labels/descriptions, échelle d'effort issue de `supportedEffortLevels`, `fastMode` depuis `supportsFastMode`, `contextWindow` retiré des lignes `[1m]` (plus de suffixe doublé dans `build_command`).
- [x] **Step 4: `xhigh` non rogné** — `supports_xhigh("opus[1m]")`, `("sonnet")`, `("default")` vrais ; `("claude-opus-4-5")` toujours faux.
- [x] **Step 5: Tests** — charge utile live capturée en fixture + test de repli legacy + message d'erreur du CLI ; `cargo test -p komet-harness` vert.
- [x] **Step 6: Smoke live contre le vrai CLI** — `cargo test -p komet-harness --test claude -- --ignored live_models_discovery` → `["default", "opus[1m]", "sonnet", "haiku"]` en 12,15s.

### Task 6: ACP — budget de sonde aligné sur le handshake

**Files:**
- Modify: `crates/harness/src/acp/mod.rs`

**Interfaces:**
- Consumes: `DEFAULT_HANDSHAKE_TIMEOUT` (120s), `AcpAgentSpec::model_discovery_timeout`.
- Produces: `DEFAULT_MODEL_DISCOVERY_TIMEOUT = DEFAULT_HANDSHAKE_TIMEOUT` pour les agents sans budget propre (hermes, grok, pi) ; opencode/cline gardent le leur.

**Mesures (machine de l'utilisateur, à chaud) :**

| Agent | `initialize` | `session/new` (liste des modèles) |
|---|---|---|
| hermes | 8,2s | **22,7s** |
| opencode | 5,4s | 19,7s |

- [x] **Step 1: Mesurer** — sonde ACP rejouée à la main (`initialize` + `session/new`) : Hermes répond en 22,7s à chaud ; l'ancienne borne de 10s échouait donc sur un CLI sain (« model discovery did not complete within 10s »).
- [x] **Step 2: Aligner** — le défaut ACP devient la borne du handshake (120s) : un agent vraiment bloqué est toujours rattrapé, un agent lent ne l'est plus.
- [x] **Step 3: Test** — `default_model_probe_matches_the_handshake_bound` verrouille l'invariant.

---

## Vérification

- `cargo check --workspace --all-targets` → **EXIT=0**
- `cargo test -p komet-harness` → **EXIT=0** (174 tests unitaires dont les nouveaux parse/ACP, suites d'intégration claude/acp complètes)
- `cargo test -p komet-engine -p komet-ui` → **EXIT=0** (172 tests engine lib dont les 2 nouveaux, 536 tests UI ; 0 échec)
- Smoke live **contre le vrai CLI** (`live_models_discovery`, `#[ignore]`) → `["default", "opus[1m]", "sonnet", "haiku"]` : l'erreur `claude supported_models returned no usable models` ne se reproduit plus. Sonde ACP mesurée à la main : hermes `initialize` 8,2s / `session/new` 22,7s.
- `cargo fmt --check` : lignes nouvelles conformes. Le dépôt présente un drift rustfmt préexistant sur des fichiers non touchés ici (`crates/doc/*`, `crates/update/src/lib.rs`, …) — non reformaté, hors périmètre.
- Non vérifié : comportement visuel du picker (pas de rendu manuel dans cette session) ; l'envoi réel d'un run avec un id d'alias (`--model opus[1m]`) n'a pas été exécuté jusqu'au tour de modèle (coût API).
- Rappel : le binaire que l'utilisateur exécutait avait encore les anciennes bornes (10s ACP, sous-type `supported_models`) — ces correctifs ne prennent effet qu'après rebuild.
