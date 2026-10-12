# Models live-only Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Le sélecteur n'affiche que les modèles réellement déclarés par chaque CLI, sans liste codée en dur.

**Architecture:** Sonde live par harness avec erreur lisible, cache persistant uniquement sur succès (écriture atomique), revalidation UI à chaque ouverture + Rescan, `komet models --check` pour la dérive.

**Tech Stack:** Rust (tokio, serde_json), GPUI pickers, clap CLI.

**Spec:** Demande utilisateur du 2026-10-11 (branche `feat/models-live-only`) : 5 points + 4 tests, sans toucher aux slash commands, diff complet avant commit, pas de push.

## Global Constraints

- Ne pas toucher aux commandes slash ni au reste.
- Pas de liste codée en dur dans le sélecteur ; seulement règles de métadonnées (ladders d'effort).
- `komet models --check` : code de sortie non nul si écart.
- Montrer le diff complet avant de committer. Pas de push.

## Review Focus

- Modèle sélectionné d'un chat restant affiché même s'il est caché → testé par le libellé « unavailable » dans le picker.
- Cache corrompu/absent au boot → le picker sonde, pas de crash.
- CLI installé mais déconnecté → état vide + raison lisible, pas de fallback statique.
- `models --check` avec un harness non installé → ignoré, pas d'échec fantôme.
- Rescan concurrent (double-clic) → single-flight, une seule sonde.

---

### Task 1: Codex catalog live-only

**Files:**
- Modify: `crates/harness/src/codex/catalog.rs`
- Test: `crates/harness/src/codex/catalog.rs` (mod tests)

**Interfaces:**
- Consumes: `~/.codex/models_cache.json`, `model/list` JSON-RPC.
- Produces: `parse_models_cache() -> Option<Vec<Model>>` (continue par entrée invalide), `is_hidden_visibility()`, `service_tiers()`.

- [x] **Step 1: visibility "list" seulement** — `hide`/`none` cachés, `hidden` gardé par compat. Fait.
- [x] **Step 2: `continue` par entrée invalide** (slug absent/vide). Fait.
- [x] **Step 3: serviceTier seulement si service_tiers non vide.** Fait.
- [ ] **Step 4: Run test** `cargo test -p komet-harness codex::catalog`. Expected: PASS.

### Task 2: Sondes live-only (claude/codex/antigravity)

**Files:**
- Modify: `crates/harness/src/claude/mod.rs`, `crates/harness/src/codex/mod.rs`, `crates/harness/src/antigravity/mod.rs`

**Interfaces:**
- Produces: `models() -> Result<Vec<Model>, HarnessError>` avec raison lisible (CLI absent, non connecté, délai dépassé). Zéro `Ok(static_models())` dans ces chemins.

- [x] **Step 1: renvoyer Err lisible sur échec.** Fait (vérifié).
- [x] **Step 2: supprimer static_models des chemins models().** Fait pour les 3 (codex garde le cache fichier local, pas de hardcodé).
- [ ] **Step 3: Run test** `cargo test -p komet-harness`. Expected: PASS.

### Task 3: Registry live-only + warm au démarrage

**Files:**
- Modify: `crates/engine/src/registry.rs`, `crates/engine/src/lib.rs`
- Test: `crates/engine/src/registry.rs` (mod tests)

**Interfaces:**
- Consumes: `Harness::models()`.
- Produces: `models()`, `warm_models_cache(ids)`, `invalidate_models(id)`, `cached_model_ids()`, `probe_live()` ; écriture atomique temp+rename.

- [x] **Step 1: persister que les listes live, retirer has_obsolete.** Fait (aucun has_obsolete restant).
- [x] **Step 2: single-flight par harness + concurrence 2.** Fait.
- [x] **Step 3: écriture atomique temp+rename.** Fait (foreground + background).
- [ ] **Step 4: appeler warm_models_cache au démarrage (installé + activé, arrière-plan).** À FAIRE dans `assemble_with_profile_locked`.
- [ ] **Step 5: Run test** `cargo test -p komet-engine registry`. Expected: PASS.

### Task 4: UI picker live-only

**Files:**
- Modify: `crates/ui/src/pickers.rs`

**Interfaces:**
- Produces: état vide + raison, bouton Rescan (`REFRESH_MODELS`), revalidation à chaque ouverture, modèle sélectionné affiché même si caché (« unavailable »).

- [x] **Step 1: état vide + raison + Rescan.** Fait.
- [x] **Step 2: re-demander à chaque ouverture.** Fait (`revalidate_models`).
- [x] **Step 3: modèle sélectionné reste affiché.** Fait.
- [ ] **Step 4: Run test** `cargo check -p komet-ui` (ou build GD). Expected: PASS.

### Task 5: `komet models --check`

**Files:**
- Modify: `apps/komet/src/main.rs`
- Test: manuel `cargo run -p komet -- models --check; echo $?`

**Interfaces:**
- Produces: par harness (installé, hors Mock) : live vs cache, bail non-nul si drift ou échec de sonde.

- [x] **Step 1: implémenter.** Fait.
- [ ] **Step 2: vérifier compilation** `cargo check -p komet`. Expected: PASS.

### Task 6: Finalisation

- [ ] **Step 1: `cargo test -p komet-harness -p komet-engine`.** Expected: PASS.
- [ ] **Step 2: `git diff` complet affiché à l'utilisateur AVANT commit.**
- [ ] **Step 3: Commit sur `feat/models-live-only`, sans push.**
