# Modèles refresh live + groupés par provider + sync CLI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ouvrir Komet, choisir un agent, voir uniquement les modèles réellement offerts par son CLI, groupés par provider, et ne jamais pouvoir envoyer un modèle que le CLI ne connaît pas.

**Architecture:** Sondes live par harness avec erreur lisible (install/auth/timeout/API-key), zéro fallback statique dans les chemins `models()`, cache persistant uniquement sur succès avec écriture atomique + single-flight, revalidation UI forcée à chaque ouverture/switch, `model_provider()` étendu comme seule règle de groupement d'affichage, send-path qui refuse un id hors-catalogue.

**Tech Stack:** Rust (tokio, serde_json), GPUI pickers, clap CLI, ACP `session/new` + `agy models` + `codex app-server model/list` + Claude control-channel.

**Spec:** Demande utilisateur du 2026-10-12 (branche `feat/transparent-cli-errors`, suite de `docs/superpowers/plans/2026-10-11-models-live-only.md`) : refresh à l'ouverture pour TOUS les agents installés, affichage par provider (Antigravity Gemini vs Claude, Cline par provider, Hermes multi-provider), messages API-key requise, sync stricte Komet ↔ CLI sans friction.

## Global Constraints

- Ne pas toucher aux slash commands ni au reste du run-path (steering, sandbox, permissions).
- Pas de liste codée en dur servie comme succès dans `models()` ; seules les règles de métadonnées (ladders d'effort, labels provider) restent statiques.
- `komet models --check` : code de sortie non nul si drift ou échec de sonde sur un harness installé.
- Montrer le diff complet avant de committer. Pas de push.
- Chaque `models()` propage `HarnessError::NotInstalled` / `Protocol(msg)` lisible avec cause (CLI absent, non connecté, timeout, clé API manquante) — jamais `Ok(static)`.

## Review Focus

- Cache corrompu/absent au boot → le picker sonde en live, pas de crash, skeleton puis liste ou erreur + Rescan.
- CLI installé mais déconnecté / clé API manquante → état vide + raison lisible ("sign in with `cline`", "`opencode auth login`", "`agy` login"), pas de fallback statique.
- Modèle sélectionné d'un chat qui disparaît du live → chip "unavailable" + envoi bloqué avec message, pas d'envoi silencieux vers le défaut.
- Rescan concurrent (double-clic ouverture) → single-flight, une seule sonde par harness.
- Liste live vide mais cache frais existant → servir le stale UNIQUEMENT avec bandeau "hors-ligne", sinon erreur (pas de stale silencieux).

---

### Task 1: Sondes live-only tous harnesses

**Files:**
- Modify: `crates/harness/src/claude/mod.rs:771-809`
- Modify: `crates/harness/src/codex/mod.rs:394-427`
- Modify: `crates/harness/src/cursor/mod.rs:255-272`
- Modify: `crates/harness/src/antigravity/mod.rs:144-174`
- Modify: `crates/harness/src/acp/mod.rs:1729-1773`
- Test: `crates/harness/tests/acp.rs`, `crates/harness/src/antigravity/catalog.rs` (mod tests), `crates/harness/src/claude/catalog.rs` (mod tests)

**Interfaces:**
- Consumes: CLI binaires (`claude`, `codex`, `cursor-agent`, `agy`, `grok`, `hermes`, `opencode`, `pi-acp`, `cline`), ACP `session/new`, `agy models`, `model/list` JSON-RPC.
- Produces: `Harness::models() -> Result<Vec<Model>, HarnessError>` avec `NotInstalled("...install hint...")` ou `Protocol("...sign in / auth / timeout...")`. Zéro `Ok(static_models())` / `Ok(load_catalog())` / `Ok((spec.models)())` dans ces chemins.

- [x] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn catalog_harness_failed_probe_returns_err_not_static() {
    let harness = ClaudeHarness::new().with_executable("/nonexistent/never-a-claude");
    let err = harness.models().await.expect_err("probe must fail");
    assert!(err.to_string().contains("claude") || matches!(err, HarnessError::NotInstalled(_)));
}
```

- [x] **Step 2: Run test to verify it fails**

Run: `cargo test -p komet-harness catalog_harness_failed_probe_returns_err_not_static -v`
Expected: FAIL (actuel renvoie `Ok(static)`)

- [x] **Step 3: Implement `models()` live-only dans `claude/mod.rs`, `codex/mod.rs`, `cursor/mod.rs`, `antigravity/mod.rs`, `acp/mod.rs`**

Supprimer chaque branche `Ok(static_models())` / `Ok(load_catalog())` / `Ok((spec.models)())` sur échec ou vide ; renvoyer `Err(HarnessError::Protocol(...))` avec raison issue du wire (ex. "Authentication required — sign in with `cline`", "`opencode auth login` requis", "`agy` login requis", "timeout 15s `agy models`") ou `NotInstalled(hint)`. Garder le cache mémoire 10s / 300s existant uniquement comme déduplication, jamais comme fallback d'erreur. Single-flight déjà présent via `models_probe` (ACP) / `OnceCell` (codex/cursor) — vérifier antigravity/claude sans double spawn.

- [x] **Step 4: Run tests to verify they pass**

Run: `cargo test -p komet-harness -v`
Expected: PASS

- [x] **Step 5: Commit**

```bash
git add crates/harness/src/claude/mod.rs crates/harness/src/codex/mod.rs crates/harness/src/cursor/mod.rs crates/harness/src/antigravity/mod.rs crates/harness/src/acp/mod.rs
git commit -m "feat(models): sondes live-only tous harnesses, erreur lisible"
```

### Task 2: Registry live + cache discipline + warm au démarrage

**Files:**
- Modify: `crates/engine/src/registry.rs:229-320`
- Modify: `crates/engine/src/lib.rs` (appel `warm_models_cache` dans `assemble_with_profile_locked`)
- Modify: `crates/engine/src/rpc.rs:1355-1368` (LIST_MODELS `force` passthrough, déjà présent — vérifier)
- Test: `crates/engine/src/registry.rs` (mod tests)

**Interfaces:**
- Consumes: `Harness::models()`.
- Produces: `models(id, force)`, `warm_models_cache(ids)`, `invalidate_models(id)`, `cached_model_ids()`, `probe_live(id)` ; écriture atomique temp+rename ; single-flight par harness + concurrence 2 au warm.

- [x] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn models_force_bypasses_cache_and_propagates_error() {
    let registry = HarnessRegistry::new();
    registry.load_models_cache(std::path::Path::new("/nonexistent-dir-xyz"));
    let err = registry.models(HarnessId::ClaudeCode, true).await.expect_err("sans CLI ProbeError attendu");
    assert!(!err.to_string().is_empty());
}
```

- [x] **Step 2: Run test to verify it fails**

Run: `cargo test -p komet-engine models_force_bypasses_cache_and_propagates_error -v`
Expected: FAIL (actuel sert stale ou static via `has_obsolete` / fallback cache)

- [x] **Step 3: Implement `models(id, force)` strict dans `crates/engine/src/registry.rs`**

Supprimer le bloc `has_obsolete` ad-hoc (lignes 247-257). Règle : `force=true` → bypass total du cache, `resolve(id)?` puis `harness.models().await?` (propager `Err`, persister uniquement si `!empty`). `force=false` → stale-while-revalidate existant mais sans `has_obsolete` : servir cache frais `< TTL`, refresh background `>= REFRESH_AFTER` avec écriture atomique, fallback stale UNIQUEMENT si `harness.models()` échoue ET cache non-vide (avec log `warn!`), sinon propager `Err`. Ajouter `warm_models_cache(ids)` (concurrence 2, single-flight par harness via `Mutex<HashSet>`) appelé au démarrage dans `assemble_with_profile_locked` pour (installé + activé, hors Mock) en arrière-plan, plus `invalidate_models(id)` et `probe_live(id)`.

- [x] **Step 4: Run test to verify it passes**

Run: `cargo test -p komet-engine registry -v`
Expected: PASS

- [x] **Step 5: Commit**

```bash
git add crates/engine/src/registry.rs crates/engine/src/lib.rs crates/engine/src/rpc.rs
git commit -m "feat(models): registry live strict, warm au boot, sans has_obsolete"
```

### Task 3: Groupement par provider + messages API-key

**Files:**
- Modify: `crates/ui/src/pickers.rs:3375-3443` (`model_provider`), `1463-1489` (tri + headers déjà présent — vérifier rendu section headers), `968-1030` (`ensure_models` erreur + Rescan)
- Test: `crates/ui/src/pickers.rs` (mod tests, ex. `model_provider_groups_antigravity_hermes_cline_opencode`)

**Interfaces:**
- Consumes: `(HarnessId, model.id)` ex. `gemini-3.8-flash-high`, `claude-sonnet-4-6`, `anthropic/claude-opus-4`, `openrouter/deepseek-v3`, `nous:hermes-4-405b`, `cline-free/deepseek-v3:free`.
- Produces: `model_provider(harness, model_id) -> &'static str` ("Google Gemini", "Anthropic", "OpenAI", "Nous Research", "xAI", "DeepSeek", "Meta", "Mistral", "Qwen / Alibaba", "Free Models", "Other") ; tri `(provider, label)` contigu pour headers ; état vide `Loadable::Error(reason)` + bouton Rescan (`REFRESH_MODELS` force=true).

- [x] **Step 1: Write the failing test**

```rust
#[test]
fn model_provider_groups_antigravity_hermes_cline_opencode() {
    assert_eq!(model_provider(HarnessId::Antigravity, "gemini-3.8-flash-high"), "Google Gemini");
    assert_eq!(model_provider(HarnessId::Antigravity, "claude-sonnet-4-6"), "Anthropic");
    assert_eq!(model_provider(HarnessId::Antigravity, "gpt-oss-120b-medium"), "OpenAI");
    assert_eq!(model_provider(HarnessId::Hermes, "nous:hermes-4-405b"), "Nous Research");
    assert_eq!(model_provider(HarnessId::Opencode, "openrouter/deepseek-v3"), "DeepSeek");
    assert_eq!(model_provider(HarnessId::Cline, "cline-free/deepseek-v3:free"), "Free Models");
}
```

- [x] **Step 2: Run test to verify it fails**

Run: `cargo test -p komet-ui model_provider_groups_antigravity_hermes_cline_opencode -v`
Expected: FAIL (`openrouter/deepseek-v3` → "Other" aujourd'hui, `nous:` couvert mais `openrouter` non)

- [x] **Step 3: Implement `model_provider()` étendu dans `crates/ui/src/pickers.rs`**

Ajouter alias vendor : `openrouter` → dériver le vrai vendor après `/` (`openrouter/deepseek-v3` → "DeepSeek", `openrouter/anthropic/claude` → "Anthropic", inconnu → "OpenRouter"), `nousresearch` en plus de `nous:`, `x-ai`/`xai` → "xAI", `google`/`gemini` → "Google Gemini" pour ACP aussi, `openai`/`gpt`/`o1`/`o3` → "OpenAI". Garder `&'static str` (pas de String). Vérifier que `visible_model_rows` trie toujours `(provider, label)` et que le rendu émet un header par groupe + que `Loadable::Error` affiche la raison (auth/API-key : "sign in", "auth login", "API key") + bouton Rescan qui rappelle `ensure_models(effective, true)`. Revalidation forcée à chaque ouverture déjà en place (`toggle` → `ensure_models(effective, true)` + `prefetch`) — ne pas régresser.

- [x] **Step 4: Run test to verify it passes**

Run: `cargo test -p komet-ui model_provider -v`
Expected: PASS (plus `cargo check -p komet-ui`)

- [x] **Step 5: Commit**

```bash
git add crates/ui/src/pickers.rs
git commit -m "feat(models): groupement provider complet + erreurs API-key lisibles"
```

### Task 4: CLI `komet models` groupé par provider + `--check`

**Files:**
- Modify: `apps/komet/src/models_cli.rs:113-202` (`render_models_table`)
- Modify: `apps/komet/src/main.rs` (flag `--check`, déjà partiellement présent sur la branche — finaliser)
- Test: manuel `cargo run -p komet -- models --check; echo $?` + `cargo run -p komet -- models antigravity`

**Interfaces:**
- Consumes: `registry.models(harness).await` (via RPC ou fallback `default_registry`).
- Produces: table groupée par `model_provider()` (header `== Google Gemini ==` puis lignes), `--json` inchangé, `--check` : live vs cache par harness installé hors Mock, sortie non-nulle si drift ou sonde en échec.

- [x] **Step 1: Write the failing check (manuel)**

Run: `cargo run -p komet -- models antigravity 2>&1 | head -20`
Expected: FAIL (actuellement table plate ID/MODEL/DESCRIPTION sans groupes provider)

- [x] **Step 2: Run to confirm**

Run: `cargo run -p komet -- models antigravity 2>&1 | head -20`
Expected: sortie plate, sans `== Google Gemini ==` / `== Anthropic ==`

- [x] **Step 3: Implement `render_models_table()` groupée dans `apps/komet/src/models_cli.rs`**

Trier par `(model_provider(harness, id), label)`, imprimer un header par groupe (`== {provider} ==`), garder `--json`/`-q` bruts. Finaliser `--check` dans `main.rs` : pour chaque harness installé hors Mock, comparer live vs cache, message par cause (absent / non connecté / drift `live != cache`), `std::process::exit(1)` si écart.

- [x] **Step 4: Run to verify it passes**

Run: `cargo check -p komet && cargo run -p komet -- models antigravity 2>&1 | head -30`
Expected: PASS + headers provider visibles

- [x] **Step 5: Commit**

```bash
git add apps/komet/src/models_cli.rs apps/komet/src/main.rs
git commit -m "feat(models): CLI groupe par provider + check drift"
```

### Task 5: Send-path synchronisé (zéro friction)

**Files:**
- Modify: `crates/ui/src/pickers.rs:734-747` (`resolved()`), `694-704` (`selected_model`), `1296-1337` (`pick_model_value` / clamp)
- Modify: `crates/engine/src/sessions.rs:475-530` (validation du model au `createChat`/send)
- Test: `crates/ui/src/pickers.rs` (mod tests) + `crates/engine/tests/e2e.rs` (send avec id hors-catalogue)

**Interfaces:**
- Consumes: `Loadable<Vec<Model>>` live du harness effectif.
- Produces: `resolved().model` = id offert uniquement, sinon `None` (harness default) ; chat existant dont le modèle a disparu → chip "unavailable" + send bloqué avec message "Modèle plus offert par le CLI — resélectionne (Rescan)" ; `sessions::createChat` rejette un `model` inconnu du live avec `ValidationError` lisible.

- [x] **Step 1: Write the failing test**

```rust
#[test]
fn resolved_never_sends_stale_model_id() {
    // models live = ["gemini-3.8-flash-high"], remembered = "gemini-2.5-flash" (disparu)
    // resolved().model doit être None ou le défaut, jamais l'id disparu.
}
```

- [x] **Step 2: Run test to verify it fails**

Run: `cargo test -p komet-ui resolved_never_sends_stale_model_id -v`
Expected: FAIL (actuel retombe sur `default_model` mais sans marquage unavailable / sans blocage send explicite)

- [x] **Step 3: Implement sync send-path dans `pickers.rs` + `sessions.rs`**

`selected_model()` : si `effective_model_id` absent du live → `default_model` + flag `stale=true` affiché "unavailable" (garder le libellé rappelé via `remember_labels`). `resolved()` : ne jamais envoyer l'id disparu (envoyer `None` → défaut harness, ou bloquer si chat existant). `sessions.rs` : au `createChat`/Run, si `request.model` non-vide et absent de `registry.models(id, false)` → `Err(ValidationError)` "model {id} not offered by {harness} CLI — Rescan" (sauf Mock/tests).

- [x] **Step 4: Run test to verify it passes**

Run: `cargo test -p komet-ui resolved_never_sends_stale_model_id -v && cargo test -p komet-engine sessions -v`
Expected: PASS

- [x] **Step 5: Commit**

```bash
git add crates/ui/src/pickers.rs crates/engine/src/sessions.rs
git commit -m "feat(models): send-path refuse les ids hors-CLI, chip unavailable"
```

### Task 6: Finalisation

- [x] **Step 1: Run full verification**

Run: `cargo test -p komet-harness -p komet-engine`
Expected: PASS (plus `cargo check -p komet-ui -p komet`)

- [x] **Step 2: Show full diff BEFORE commit**

Run: `git diff --stat && git diff`
Expected: relu par l'utilisateur, sans push

- [x] **Step 3: Commit de clôture si reliquat**

```bash
git add -A
git commit -m "feat(models): refresh live global + providers + sync CLI"
```

## Self-Review

1. Spec coverage: refresh global (T1+T2) oui ; groupement provider (T3+T4) oui ; messages API (T1+T3) oui ; sync friction zéro (T5) oui ; `models --check` (T4) oui.
2. Step scan: chaque step = un test nommable OU une commande vérifiable OU une signature exacte + fichier ; pas de "gérer les edge cases" générique.
3. Type consistency: `Harness::models() -> Result<Vec<Model>, HarnessError>`, `registry.models(id, force)`, `model_provider(HarnessId, &str) -> &'static str`, `resolved() -> ResolvedRunConfig` — cohérents entre tâches.
4. Review Focus: les 5 risques listés ont chacun un test dans leur tâche propriétaire.
5. Proportion: plan ≈ spec, pas de corps de fonction recopié — l'implémenteur écrit les corps.
