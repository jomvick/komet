# Widgets Composer (Changes / Sous-agents / Processus) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Trois chips au-dessus du composer (Changes → panneau Diff, Sous-agents → popup/liste, Processus → popup des commandes agent), remplaçant la surface `AgentShell` du panneau droit.

**Architecture:** Strip Shell-owned rendu enfant flex au-dessus de `self.composer` (content.rs:156). Dérivation au paint des données existantes (`agent_shell_log`, chips sous-agents + `subagent_live`, entité `Changes` headless). Un `Popup<WidgetKind>` partagé (motif `Popup<PickerKind>` de pickers.rs). Zéro changement/proto/moteur/harnais.

**Tech Stack:** Rust / gpui (workspace komet, sucre `komet-ui`).

**Spec:** `docs/superpowers/specs/2026-11-03-composer-widgets-design.md`

## Global Constraints

- Lecture seule, toujours : aucune écriture vers le shell de l'agent, pas de Stop, pas de durée par commande (§Non-goals du design).
- `crates/harness` et `crates/engine` ne sont pas touchés ; la matrice de capacités (`interaction_surface(id).subagents` / `.tool_progress`) reste telle quelle.
- Visibilité exacte : Changes caché sans diff non vide ; Sous-agents jamais si `subagents == None` ; Processus jamais si `tool_progress == None` ; hystérésis masquage Changes 600 ms ; linger done 6 s ; chip processus reste tant que ≥ 1 commande `Failed`.
- Ordre des chips left→right : Changes, Sous-agents, Processus.
- Pill : h 24 px, `rounded_full`, bordure `theme.border`, fond `ink(0.04)` → `0.07` hover → `0.10` open, texte 11,5 px, padding h 9 px, gap 6 px, chevron bas sur les deux chips à popup. Compteurs `+N` en `theme.success`, `−M` en `theme.danger`.
- Popup : largeur 340 px, max-h 320 px, `overflow_y_scroll`, ancrée 6 px au-dessus du coin haut-gauche du chip, réserve anti-clip 8 px du bord de fenêtre, animations `menu_in`/`menu_out` existantes, fermeture Échap + clic extérieur (`on_mouse_down_out`).
- `KOMET_*` : rien de nouveau ; pas de dépôt de nouvelles dépendances.
- Conventional Commits avec scope : `feat(ui): …`, `refactor(ui): …`, `test(ui): …`.

## Review Focus

1. **Changement de chat sélectionné** pendant une popup ouverte → la popup ferme et les compteurs reset au chat courant. Test : pure fn de reset du strip (Task 5).
2. **Harnais sans capability** (`subagents: none` / `tool_progress: none`) → chip jamais rendu, même si des données arrivent. Test : pure fn de gating (Task 5).
3. **Sortie vide / vieux transcripts sans rows Exec ou sans spawn chips** → listes vides → chip caché, pas de panique dans la popup. Test : derivations pures (Tasks 3–4).
4. **Watch diff indisponible / erreur** → chip Changes caché (pas d'affichage d'erreur dans une pill). Test : `summary` sur slot en erreur (Task 6).
5. **« Voir les commandes » du transcript** (badge d'un Run chip) → ouvre la popup Processus du chat courant, jour et nuit, sans créer d'onglet. Test : pure fn de retarget (Task 5).

---

### Task 1: Module `composer_widgets` + pures (format_count, summarize_diff)

**Files:**
- Create: `crates/ui/src/composer_widgets.rs`
- Modify: `crates/ui/src/lib.rs` (ajouter `pub mod composer_widgets;`)
- Test: dans `crates/ui/src/composer_widgets.rs` (`#[cfg(test)] mod tests`), style `transcript.rs`.

**Interfaces:**
- Consumes: `crate::changes::FileDiff` (pub, fields `additions: u32`, `deletions: u32` — changes.rs:162).
- Produces (Tasks 4–6):
  - `pub fn format_count(n: u32) -> String` — `≤ 999` tel quel ; `≥ 1000` → un décimal, `.0` tronqué (`1000` → `1k`, `1200` → `1.2k`, `15190` → `15.2k`).
  - `pub fn summarize_diff(files: &[crate::changes::FileDiff]) -> Option<(usize, u32, u32)>` — `None` si 0 fichiers (clean), sinon `(fichiers, additions, suppressions)`.

- [ ] **Step 1: Write the failing tests** — `format_count` (999, 1000, 1200, 15190), `summarize_diff` (0 fichier → None ; 2 fichiers 5+/3− → Some((2,5,3))).
- [ ] **Step 2: Run** `cargo test -p komet-ui format_count summarize_diff` — Expected: FAIL (« cannot find function »).
- [ ] **Step 3: Implement** les deux fn dans `composer_widgets.rs` (corps triviaux ; `format_count` : `if n >= 1000 { format!("{:.1}k", n as f32 / 1000.0).trim_end_matches(".0").replace(".0k","k") } `— écrire proprement, test-first).
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `test(ui): composer widgets pure helpers`

### Task 2: Machine à états de visibilité `LingerGate`

**Files:**
- Create: `crates/ui/src/composer_widgets.rs` (même module, section `Visibility`).

**Interfaces:**
- Produces (Tasks 4–5):
  - `pub struct LingerGate { shown: bool, until: Option<std::time::Instant>, linger: std::time::Duration }` — `linger` vaut 600 ms pour Changes, 6 s pour les deux autres.
  - `pub fn step(&mut self, want: bool, pin: bool) -> bool` — `pin` (`true` = ≥1 failed) force `shown` au-dessus de `want` ; une transition shown→hidden n'arrive qu'à l'expiration de `until` (reset de `until` dès que `want` redevient vrai) ; premier `want=true` → montré immédiatement.
  - `pub fn wake_in(&self) -> Option<std::time::Duration>` — Some quand `until` est en cours (pour armer le re-notify, Task 5), None sinon.

- [ ] **Step 1: Write the failing tests** — wants: on→visible immédiat ; off→reste visible ≥ `wake_in()` puis caché après expiration (avec instants injectés via paramètre `now` — passer `now: Instant` à `step` pour la testabilité) ; re-on avant expiration ; `pin=true` garde visible ; deux gates avec `linger` différents.
- [ ] **Step 2: Run** — Expected: FAIL.
- [ ] **Step 3: Implement `LingerGate`** (signature finale : `pub fn step(&mut self, want: bool, pin: bool, now: Instant) -> bool`).
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `test(ui): widget visibility gates`

### Task 3: Dérivations de données côté Transcript

**Files:**
- Modify: `crates/ui/src/transcript.rs` — `pub struct SubagentRowData` + dérivation pure `pub fn subagent_row_data(rows: &[Row], live: &HashMap<SharedString, SubagentLive>) -> Vec<SubagentRowData>` + accesseur `pub fn subagent_rows(&self, cx: &App) -> Vec<SubagentRowData>` (non-pur : il joint la cible `remote_host(cx)`, cf. transcript.rs:5263), placés autour de `agent_shell_log` (3679).
- Test: `#[cfg(test)] mod tests` de transcript.rs (à côté des tests `agent_shell_entries`, 6886+).

**Interfaces:**
- Consumes: **un spawn n'a PAS de variante `ToolCall` dédiée** — c'est tout tool dont `tool.subagent_status: Option<SubagentStatus>` est `Some` ; ses données sont `tool.part_id` (= `parent_tool_use_id`), `tool.chip_label` (= title), `tool.call.subagent_model()` (= model), `tool.subagent_duration_ms`, `tool.subagent_tool_count` (motif de construction transcript.rs:5264-5270). Live par `subagent_live` (transcript.rs:2093), clé = part_id.
- Produces (Task 5):
  ```rust
  pub struct SubagentRowData {
      pub spawn: SubagentSpawn,        // target_device_id = self.remote_host(cx)
      pub live: Option<SubagentLive>,  // gagne sur le statut chip tant que running
  }
  ```
  Triées running en tête puis done/failed (ordre doc), max 5 (les plus récentes d'abord).
- Les processus n'ont **rien à ajouter** ici : Task 5 lit `self.agent_shell_log()` tel quel.

- [ ] **Step 1: Write the failing tests** — rows sans chip subagent → vec![] ; chip, pas de live → statut du chip ; chip + live → live gagne pour running ; cap 5 avec running en tête ; deux tools distincts avec des part_id distincts → deux rangées.
- [ ] **Step 2: Run** `cargo test -p komet-ui subagent_rows` — Expected: FAIL.
- [ ] **Step 3: Implement** `subagent_row_data` + l'accesseur (`SubagentOpen` complet reconstruct exactement comme transcript.rs:5264-5276).
- [ ] **Step 4: Run** — Expected: PASS. Puis `cargo test -p komet-ui` entier — Expected: PASS.
- [ ] **Step 5: Commit** `feat(ui): transcript subagent rows derivation`

### Task 4: État du strip + contenu des popups (pur)

**Files:**
- Modify: `crates/ui/src/composer_widgets.rs` —
  ```rust
  pub enum WidgetKind { Subagents, Processes }
  pub struct WidgetStrip {
      pub open: crate::popover::Popup<WidgetKind>,     // un seul, partagé par la barre
      pub changes_gate: LingerGate,   // linger 600 ms
      pub subagents_gate: LingerGate, // linger 6 s
      pub processes_gate: LingerGate, // linger 6 s
  }
  ```
  Méthodes : `open_popup(&mut self, kind)`, `close(&mut self, task: &Task-returning closure)` non — **la fermeture passe par le motif pickers** : `begin_close` + `popover::reap_popup` (popover.rs:133/185, attesté par `pickers.rs::animate_close` 885-890) ; `reset(&mut self)` (changement de chat : ferme + gates reset) ; `wake_in(&self) -> Option<Duration>` (max des deadlines des gates).
- Test: pures du module (contenu des rangées, pas de tests de layout gpui).

**Interfaces:**
- Consumes: `popover::Popup<T>` (API : `open`, `get`, `as_open`, `note_trigger_press_matching`, `take_press_was_open`, `begin_close`, `closing_since` — popover.rs:67-205) ; `anchored_menu_above(id, element, closing)` ; Task 1–3.
- Produces (Task 5):
  ```rust
  pub struct StripInput {
      pub widget_changes: Option<(usize, u32, u32)>,   // None = clean/erreur → caché
      pub subagent_rows: Vec<crate::transcript::SubagentRowData>,
      pub process_entries: Vec<crate::transcript::AgentShellEntry>,
      pub subagents_allowed: bool,   // interaction_surface().subagents != None
      pub processes_allowed: bool,   // interaction_surface().tool_progress != None
  }
  pub struct ChipContent {           // construit/validé par les pures
      pub visible: bool,
      pub icon: &'static str,        // crate::icons::*
      pub label: SharedString,       // "3 files +12 −4" / "N agents" / "N processes"
  }
  pub fn chip_content(kind: WidgetKind, input: &StripInput, now: Instant...) -> ChipContent; // figé par les tests
  pub fn process_row_lines(entry: &AgentShellEntry) -> (String command_truncated, Option<SharedString> last_line);
  ```
  Les **listeners gpui (clics) sont posés en Task 5** : Task 4 ne pos que le contenu et l'état (`open`, gates, press-note via `note_trigger_press_matching`).
- Rangées popup processus : pastille (Running pulsant / Done success / Failed danger), commande monospace m-tronquée `pub fn truncate_middle(s: &str, max: usize) -> String` (44 chars, `…` central), dernière ligne d'`entry.output` en `theme.text_muted`, entrée sans sortie → note reprise de `agent_shell.rs::no_output_note`.
- Rangées popup sous-agents : pastille + `spawn.title` + `spawn.model` + `subagent_status_label(spawn.status, live, spawn.duration_ms, spawn.tool_count)`, clic → ferme la popup (Task 5).
- Échap : `track_focus` + `on_key_down` → fermeture (motif `popover_frame` pickers.rs 2811).

- [ ] **Step 1: Write the failing tests** (pures) — `truncate_middle` (court, exactement 44, dépassement → `…` central) ; `chip_content` : labels `1 file +3 −1` / `2 files +5 −3` (singulier/pluriel : "file"/"files"), `1 agent` / `N agents` / `N processes` ; gating (`allowed=false` → invisible) ; `process_row_lines` (output vide → last_line None, jamais de panique).
- [ ] **Step 2: Run** — Expected: FAIL.
- [ ] **Step 3: Implement** `WidgetKind`, `WidgetStrip`, `chip_content`, `truncate_middle`, `process_row_lines` (réutilisant `format_count` + `LingerGate` de Task 2).
- [ ] **Step 4: Run** — Expected: PASS puis `cargo check -p komet-ui`.
- [ ] **Step 5: Commit** `feat(ui): widget strip state and popup content`

### Task 5: Intégration Shell — strip, popup unique, retarget, horloge

**Files:**
- Modify: `crates/ui/src/shell.rs` (champs + handlers + retarget), `crates/ui/src/shell/content.rs:156` (le strip comme enfant flex au-dessus de `self.composer`), `crates/ui/src/composer_widgets.rs` pour le rendu gpui `render_strip`.
- Test: `crates/ui/src/shell/tests.rs` (parties pures : reset au changement de chat, retarget, gating).

**Interfaces:**
- Consumes: Task 1–4 (`WidgetStrip`, `StripInput`, `ChipContent`, `LingerGate`) ; Task 6 (`self.widget_changes.summary(cx)`, cf. ordonnancement) ; `transcript::agent_shell_log()` + `subagent_rows(cx)` (Task 3) ; `komet_harness::capabilities::interaction_surface(id)` (pattern content.rs:800-810).
- Produces:
  - champs Shell : `widgets: WidgetStrip` ; `widgets_wake: Option<Task<()>>` (one-shot armé quand `strip.wake_in()` est `Some` : timer → `cx.notify()` — motif `provider_switch_task` pickers.rs:1442) ; `widget_changes: Option<Entity<Changes>>` (Task 6).
  - `pub(super) fn open_changes_widget(&mut self, cx: &mut Context<Self>)` — idempotent, **sans toggle** : si la pane est déjà ouverte sur `RightSurface::Diff` du checkout courant → no-op ; sinon si un `Diff(id)` existe dans `right_tabs[panel_key]` → `set_right_active(Diff(id))` + réveil de la pane (motif open-if-closed right_pane.rs:745-749) ; sinon `add_diff_surface` (Task 6 en produit un ; le panneau s'ouvre via `register_diff_surface` right_pane.rs:683-703).
  - `pub(super) fn open_subagent_tab(&mut self, open: SubagentOpen, cx)` — délègue à `add_subagent_surface` (right_pane.rs:707), ferme déjà la popup côté strip.
  - rendu : `render_strip(self, input: StripInput, cx: &mut Context<Shell>)` dans `composer_widgets.rs` — pose les listeners gpui : chip Changes → `open_changes_widget` ; chips Subagents/Processus → press-note + toggle (motif `pickers.rs::trigger_chip` 2403-2409) ; rangée sous-agent → `open_subagent_tab` ; rangée processus → clic stoque rien (lecture seule).

- [ ] **Step 1: Write the failing tests** (shell/tests.rs, pures) :
  - changement de chat : nouvelle fn pure `WidgetStrip::reset_checked(&mut self, new_chat: &str, old_chat: &str)` — même chat → rien ; autre chat/None → popup fermée + gates reset.
  - gating : `fn chip_allowed(subagents: SubagentSurface, tool_progress: ProgressSurface, kind: WidgetKind) -> bool`.
  - retarget : `fn agent_shell_retarget(kind_open) -> WidgetKind` — l'événement n'ouvre QUE `WidgetKind::Processes` comment-à-faire.
- [ ] **Step 2: Run** — Expected: FAIL.
- [ ] **Step 3: Implement** — champs Shell + `render_strip` + `open_changes_widget` + `open_subagent_tab` + retarget du handler shell.rs:513 → `widgets.open_popup(Processes)` (chat courant : l'événement vient du transcript sélectionné, rien à ajouter) + arming du `widgets_wake` quand `strip.wake_in()` est `Some`.
- [ ] **Step 4: Run** `cargo test -p komet-ui` — Expected: PASS. `cargo check -p komet-ui` — Expected: propre.
- [ ] **Step 5: Commit** `feat(ui): wire the widget strip into the shell`

### Task 6: Entité `Changes` headless + summary + create-or-reuse

**Files:**
- Modify: `crates/ui/src/changes.rs` — **un seul ajout pub** :
  ```rust
  pub fn summary(&self, cx: &App) -> Option<(usize, u32, u32)>
  ```
  `None` si watch en erreur / clean / parse vide ; sinon le `resolve_diff(...)` working-tree rend (fichiers, additions, suppressions) — pure agrégation sur les `FileDiff`. L'hystérésis de masquage vit ENTIÈREMENT dans `LingerGate` (Task 2) : **pas d'horloge dans changes.rs**.
- Modify: `crates/ui/src/shell/right_pane.rs:667-703` — `add_diff_surface` réutilise l'entité du strip : si `self.widget_changes` est `Some(entity)`, `register_diff_surface(entity.take(), cx)` et remettre `widget_changes = None` (le watch vit sur l'entité, chainable) ; sinon créer + stocker la nouvelle entity dans `widget_changes` aussi (le strip la lit et `add_diff_surface` re-registre la même). `Changes::for_commit` (674) et les autres créations restent sans partage.
- Test: `changes.rs` tests purs + shell/tests.rs (create-or-reuse).

**Interfaces:**
- Produces (Task 5): `Entity<Changes>::summary(cx) -> Option<(usize, u32, u32)>`.

- [ ] **Step 1: Write the failing tests** (changes.rs tests purs) — pas de diffs résolus → None ; diffs vides → None ; 2 fichiers (3+ / 2−) et (2+ / 1−) → `Some((2, 5, 3))`.
- [ ] **Step 2: Run** — Expected: FAIL.
- [ ] **Step 3: Implement** `summary` (agrégat sur la résolution working-tree active) ; `add_diff_surface` create-or-reuse (register/take comme explicité ci-dessus, testé par un test shell pure `diff_surface_reuses_strip_entity`).
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `feat(ui): headless changes entity feeds the changes widget`

### Task 7: Retrait de `RightSurface::AgentShell` + `add_agent_shell_surface` + surface picker card

**Files:**
- Modify: `crates/ui/src/shell/navigation.rs:93` (variante), `crates/ui/src/shell/right_pane.rs:753-771` (`add_agent_shell_surface` supprimée), `content.rs:794-817` (match de surface → la branche tombe dans `_ => render_surface_picker(cx)` existant) et `content.rs:1011-1015` (carte « Shell agent » du surface picker), icône de tab `content.rs:1119-1122`.
- Delete: `crates/ui/src/agent_shell.rs` + `pub mod agent_shell;` retiré de `lib.rs` (les helpers dont la popup a besoin ont déjà migré dans `composer_widgets.rs` au Task 4 ; **on ne touche pas** à `agent_shell_entries` et à ses tests purs dans transcript.rs).

- [ ] **Step 1: retirer** tous les usages listés ; puis `grep -rn AgentShell crates/ui` ne doit retourner que `OpenAgentShell` (nom d'événement gardé, lecture « ouvre la popup processus » depuis Task 5).
- [ ] **Step 2: Run** `cargo check -p komet-ui --all-targets` — Expected: propre, aucun match non-exhaustif.
- [ ] **Step 3: `cargo test -p komet-ui`** — Expected: PASS.
- [ ] **Step 4: Commit** `refactor(ui): drop the AgentShell right surface — the processes widget replaces it`

### Task 8: Vérification workspace

**Files:** aucun — phase de vérification.

- [ ] `cargo check --workspace --all-targets` — Expected: propre.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` — Expected: propre.
- [ ] `cargo test --workspace` — Expected: PASS.
- [ ] `cargo fmt --all` — Expected: sans diff (ou fix inclus au dernier commit `chore(ui): fmt`).
- [ ] Vérification manuelle du design (§Plan de vérification du spec) via `cargo run --bin komet` ou `scripts/dev-demo.sh`.

---

## Ordonancement

**1 → 2 → 3 → 4 → 6 → 5 → 7 → 8.** Le strip (Task 4) est écrit avant le retrait d'`AgentShell` (Task 7) pour que le build reste vert à chaque commit ; Task 5 consomme Task 6 (`widget_changes.summary`) — implémenter Task 6 avant Task 5, sinon Task 5 rend le chip Changes avec `widget_changes: None` et l'attend.
