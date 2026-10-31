# Settings → Providers Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fusionner les pages de réglages « Agents » et « Accounts » en une seule page « Providers » : une carte cliquable par agent, dépliable, avec toggle d'activation, état CLI, comptes (Switch/Forget) et boutons d'authentification — sans jauges d'usage.

**Architecture:** `crates/ui/src/settings/accounts.rs` devient `providers.rs` et absorbe toute la logique de `harnesses.rs` (chargement `ListHarnesses`, statut CLI, toggle, install/update) : une seule entité `ProvidersPage`, un seul `target_device`, deux `Loadable` (harnesses, comptes) rendus ensemble. `harnesses.rs` est supprimé et il ne reste qu'une section de sidebar.

**Tech Stack:** Rust (edition 2024), gpui, `komet-proto` / `komet-rpc` (RPC JSON existants, aucun changement proto), tests unitaires purs dans `crates/ui`.

**Spec:** `docs/superpowers/specs/2026-10-30-settings-providers-design.md`

## Global Constraints

- Aucun changement proto/engine : les méthodes RPC existantes (`ListHarnesses`, `GetHarnessCliStatus`, `ManageHarnessCli`, `SetHarnessEnabled`, `ListAgentAccounts`, `StartAgentLogin`, `CompleteAgentLogin`, `PollAgentLogin`, `CancelAgentLogin`, `ActivateAgentAccount`, `ForgetAgentAccount`) restent telles quelles.
- `ListAgentAccounts` est toujours appelé avec `"usageMode": "none"` — plus aucune sonde d'usage.
- Les ids gpui des lignes de compte restent basés sur l'id de compte (`account-switch-{id}`, `account-forget-{id}`).
- Les handlers enfants (toggle, Connect, Install/Update) appellent `cx.stop_propagation()` : sinon le clic déplie aussi la carte.
- Vérification par tâche : `cargo check -p komet-ui --all-targets`, `cargo test -p komet-ui --lib settings::`, puis en fin de plan `cargo fmt --all`, `cargo clippy -p komet-ui --all-targets -- -D warnings`.
- Commits en Conventional Commits avec scope (`refactor(settings): …`, `feat(settings): …`).
- Le mock harness reste filtré par `crate::pickers::visible_harnesses` (aucun nouveau filtre).

## Review Focus

- **Agent sans CLI installé** : carte présente, toggle inerte, bouton Install affiché, aucun panic (le body déplié montre l'instruction d'installation).
- **`ListAgentAccounts` en échec** : les cartes restent affichées avec l'état CLI ; bande d'erreur page dédiée aux comptes ; aucune mention « Not connected » quand la lecture a échoué.
- **Clic sur toggle / Connect / Install** : la carte ne se déplie pas (propagation stoppée).
- **Dernier agent actif** : toggle inerte (garde existante `last_enabled`).
- **Device distant sélectionné** : toggle, Switch/Forget et login visent la machine affichée (résolution `harness_target_device()` à chaque RPC).

---

### Task 1: Retirer les jauges d'usage de la page des comptes

**Files:**
- Modify: `crates/ui/src/settings/accounts.rs` (suppressions ; la page s'appelle encore `AccountsPage`)
- Test: `crates/ui/src/settings/accounts.rs` (module `tests`)

**Interfaces:**
- Consumes: rien des tâches suivantes.
- Produces: `accounts.rs` sans `UsageLevel`, `usage_level`, `usage_color`, `LoadTrigger`, `usage_mode_for`, `format_reset`, `displayed_usage`, `supports_usage_probe`, `retain_previous_usage`, `render_usage_meter`, sans `AgentUsageWindow`/`UsageMode` importés. Les appels `ListAgentAccounts` passent `serde_json::json!({ "usageMode": "none" })`.

- [ ] **Step 1: Supprimer les helpers et leurs tests**

Retirer du fichier : `USAGE_WARN_FRACTION`, `USAGE_CRITICAL_FRACTION`, `enum UsageLevel`, `usage_level`, `usage_color`, `enum LoadTrigger`, `usage_mode_for`, `format_reset`, `displayed_usage`, `supports_usage_probe`, `retain_previous_usage`, `render_usage_meter`, les imports `chrono::{DateTime, Utc}` et le paramètre `now: DateTime<Utc>` de `render_account_row` (plus de reset time à formater), `komet_engine::UsageMode`, `AgentUsageWindow`. Retirer les tests `usage_thresholds_match_komet`, `usage_colors_map_to_theme_accents`, `usage_is_only_displayed_for_the_active_account`, `switching_moves_the_active_usage_to_the_selected_account`, `empty_refresh_keeps_the_last_known_usage_for_each_account`, et les tests de `usage_mode_for`.

- [ ] **Step 2: Remplacer les appels de chargement**

Les cinq appels `self.load(usage_mode_for(...), cx)` deviennent `self.load(cx)`. La signature devient `fn load(&mut self, cx: &mut Context<Self>)` et construit ses params avec `serde_json::json!({ "usageMode": "none" })`. Supprimer le paramètre `mode` et le `retain_previous_usage(&mut snapshot, previous.as_ref())` associé.

- [ ] **Step 3: Simplifier la ligne de compte**

Dans `render_account_row`, supprimer le bloc des jauges et le texte « Usage unavailable » / « Credentials unavailable » : la colonne centrale ne garde que `widgets::row_title(theme, email)`. Dans `render_skeleton_row`, supprimer les deux fantômes de jauges (garder l'avatar, la ligne d'email, le badge).

- [ ] **Step 4: Vérifier**

Run: `cargo check -p komet-ui --all-targets`
Expected: aucune erreur ; plus aucune référence à `usage` dans `accounts.rs` (`grep -n "usage" crates/ui/src/settings/accounts.rs` → seulement `usageMode` dans les params).

Run: `cargo test -p komet-ui --lib settings::accounts`
Expected: PASS (tests restants : `codex_on_path`, `select_active_account`, titres de login, etc.)

- [ ] **Step 5: Commit**

```bash
git add crates/ui/src/settings/accounts.rs
git commit -m "refactor(settings): drop usage meters from the accounts page"
```

---

### Task 2: Renommer le module et le type de page (sans changement de comportement)

**Files:**
- Rename: `crates/ui/src/settings/accounts.rs` → `crates/ui/src/settings/providers.rs`
- Modify: `crates/ui/src/settings.rs:32,38` (liste `pub mod`)
- Modify: `crates/ui/src/shell.rs` (import, champ, `settings_outlet`, `settings_sub`)

**Interfaces:**
- Consumes: `accounts.rs` de la Task 1.
- Produces: `pub struct ProvidersPage`, `pub enum ProvidersEvent` (temporairement `FocusProvider(HarnessId)`), `crate::settings::providers::{blurb, cli_name, requires_login, supports_in_app_login, cli_login_command, login_instructions, PROVIDERS}`.

- [ ] **Step 1: Renommer**

```bash
git mv crates/ui/src/settings/accounts.rs crates/ui/src/settings/providers.rs
```

Dans le fichier : `AccountsPage` → `ProvidersPage`, `AccountsEvent` → `ProvidersEvent`, en-tête de module mis à jour (« Settings → Providers »). Mettre à jour `crates/ui/src/settings.rs` (`pub mod accounts;` → `pub mod providers;` en gardant l'ordre alphabétique) et `crates/ui/src/shell.rs` (`use crate::settings::providers::ProvidersPage;`, `accounts_page: Option<Entity<ProvidersPage>>` → `providers_page`).

- [ ] **Step 2: Vérifier**

Run: `cargo check -p komet-ui --all-targets`
Expected: PASS. La page s'appelle encore « Accounts » dans la sidebar, le comportement est identique.

- [ ] **Step 3: Commit**

```bash
git add -A crates/ui/src/settings crates/ui/src/shell.rs crates/ui/src/composer.rs
git commit -m "refactor(settings): rename accounts module to providers"
```

---

### Task 3: Absorber la logique Agents dans `ProvidersPage`

**Files:**
- Modify: `crates/ui/src/settings/providers.rs` (état + chargements + rendu de carte)
- Delete: `crates/ui/src/settings/harnesses.rs`
- Modify: `crates/ui/src/settings.rs` (retirer `pub mod harnesses;`)
- Modify: `crates/ui/src/shell.rs` (imports, champs, `SettingsSection`, `settings_outlet`, routes)
- Modify: `crates/ui/src/composer.rs:3866` (`settings::harnesses::cli_name` → `settings::providers::cli_name`)
- Test: `crates/ui/src/settings/providers.rs`

**Interfaces:**
- Consumes: `ProvidersPage` (Task 2).
- Produces:
  - `pub fn blurb(harness: HarnessId) -> &'static str`
  - `pub fn cli_name(harness: HarnessId) -> &'static str`
  - `fn harness_display_name(harness: HarnessId) -> &'static str`
  - `struct AgentConnectionStatus { connected: HashSet<HarnessId>, unknown: HashSet<HarnessId> }` (déjà présent, `From<AgentAccountsSnapshot>`)
  - `fn show_connect_button(harness: HarnessId, status: Option<&AgentConnectionStatus>) -> bool`
  - `fn newly_available_cli_update(statuses: &[HarnessCliStatus], announced: &HashSet<(HarnessId, String)>, enabled: &HashSet<HarnessId>) -> Option<HarnessCliStatus>`
  - `fn account_summary(accounts: &[&AgentAccount]) -> SharedString` → `"2 accounts · Active: a@b"`, `"1 account · Not connected"`, ou `""` sans compte.
  - `fn toggle_interactive(installed: bool, enabled: bool, enabled_count: usize) -> bool` → `installed && !(enabled && enabled_count == 1)`.
  - Champs `ProvidersPage`: `harnesses: Loadable<Vec<HarnessDescriptor>>`, `accounts: Loadable<AgentAccountsSnapshot>`, `cli_status: HashMap<HarnessId, HarnessCliStatus>`, `cli_update_prompt: Option<HarnessCliStatus>`, `announced_cli_updates: HashSet<(HarnessId, String)>`, `checking_updates: bool`, `managing_cli: Option<HarnessId>`, `toggle_task`, `cli_status_task`, `manage_cli_task`, `target_generation: u64`, `error: Option<SharedString>`, `accounts_error: Option<SharedString>`.

- [ ] **Step 1: Écrire le test du résumé de comptes**

```rust
#[test]
fn account_summary_names_the_active_account() {
    let account = |id: &str, email: &str, active: bool| AgentAccount {
        id: id.into(),
        harness: HarnessId::ClaudeCode,
        email: Some(email.into()),
        display_name: None,
        plan_label: None,
        active,
        switchable: true,
        usage_windows: vec![],
    };
    let active = account("a", "me@x.dev", true);
    let other = account("b", "other@x.dev", false);
    assert_eq!(
        account_summary(&[&active, &other]).to_string(),
        "2 accounts · Active: me@x.dev"
    );
    assert_eq!(
        account_summary(&[&other]).to_string(),
        "1 account · Not connected"
    );
    assert_eq!(account_summary(&[]).to_string(), "");
}
```

(Ajouter les champs manquants au littéral selon la définition réelle de `AgentAccount` dans `crates/proto/src/agent.rs` — ne pas changer le proto.)

- [ ] **Step 2: Vérifier l'échec**

Run: `cargo test -p komet-ui --lib settings::providers::tests::account_summary_names_the_active_account`
Expected: FAIL — `account_summary` non définie.

- [ ] **Step 3: Implémenter `account_summary` et `toggle_interactive`**

Prendre `accounts.first()` actif pour l'email (`email` sinon `display_name` sinon `"Unknown account"`), pluriel `account`/`accounts`. `toggle_interactive` reprend la garde existante `last_enabled` (le dernier agent actif ne peut pas être désactivé).

- [ ] **Step 3b: Écrire et faire passer le test du dernier agent**

```rust
#[test]
fn the_last_enabled_agent_cannot_be_switched_off() {
    assert!(!toggle_interactive(true, true, 1));
    assert!(toggle_interactive(true, true, 2));
    assert!(toggle_interactive(true, false, 1));
    assert!(!toggle_interactive(false, false, 3));
}
```

Run: `cargo test -p komet-ui --lib settings::providers::tests::the_last_enabled_agent_cannot_be_switched_off`
Expected: PASS après implémentation.

- [ ] **Step 4: Porter l'état et les chargements**

Copier depuis `harnesses.rs` : `AgentConnectionStatus` + `From<AgentAccountsSnapshot>` (s'il n'existe pas déjà dans `providers.rs`), `show_connect_button`, `newly_available_cli_update`, `harness_display_name`, `blurb`, `cli_name`, et les méthodes `load_harnesses`, `load_cli_status(check_latest, force)`, `load_accounts`, `load_connection_status` (supprimée : le statut de connexion se dérive de `self.accounts`), `enabled_harness_ids`, `check_updates`, `render_cli_update_dialog`, `toggle`, `manage_cli`. Un seul `with_target` :

```rust
fn with_target(&self, value: serde_json::Value, cx: &App) -> serde_json::Value
```
qui résout `self.target_device.clone().or_else(|| self.state.read(cx).harness_target_device())`.

`new` lance `load_harnesses` ; `load_harnesses` lance ensuite `load_cli_status(false, false)` et `load_accounts` quand la liste est `Ready`. `accounts_error` reçoit l'échec de `ListAgentAccounts` ; `error` celui de `ListHarnesses`/CLI/toggle.

- [ ] **Step 5: Rendre une carte par harness**

`fn rows(&self, cx) -> Vec<AnyElement>` : itérer `visible_harnesses(&descriptors)`, et pour chaque harness construire :

- l'en-tête (tile 36px avec marque + nom + `blurb` + `widgets::meta_line` des statuts CLI + ligne `account_summary`), bouton `Connect` (si `show_connect_button`) qui appelle `self.start_login(harness, cx)`, bouton Install/Update (si `show_manage_button`) qui appelle `manage_cli`, puis `widgets::toggle_switch` (interactif si `installed && !last_enabled`).
- le corps : `widgets::warning_strip` par warning du provider, `render_account_row` par compte, et le pied « Add account » (`supports_in_app_login`) ou l'instruction CLI + « Copy sign-in command » (`cli_login_command`), ou rien (Mock).
- conteneur : `widgets::section_card(&theme)` par carte, `.id(("provider-card", ix))`.

En-tête de page : titre « Providers », bouton Refresh (`load_harnesses` + `load_accounts`), bouton « Check for updates » (`check_updates`), device switcher existant (garder la version `popover::Popup` de `providers.rs`, supprimer celle de `harnesses.rs`).

- [ ] **Step 6: Supprimer `harnesses.rs` et recâbler le shell**

```bash
git rm crates/ui/src/settings/harnesses.rs
```

Dans `settings.rs`, retirer `pub mod harnesses;`. Dans `shell.rs` :

- `SettingsSection` : remplacer `Harnesses` et `Agents` par `Providers` (label « Providers », icône `icons::WIDGET`), `ALL` passe à 10 entrées.
- Champs : `providers_page: Option<Entity<ProvidersPage>>` (supprimer `accounts_page`, `harnesses_page`, `harnesses_sub`, `pending_account_harness`).
- `settings_outlet` : une branche `Providers` qui recrée la page à chaque visite (`self.providers_page = None` dans `open_settings` quand `section == Providers`).
- Routes `KOMET_OPEN_ROUTE` : `settings/providers`, plus les alias `settings/agents` et `settings/harnesses` → `Providers`.
- `ErrorOpenAccounts` : `open_settings(SettingsSection::Providers, cx)` ; supprimer `open_accounts_for_harness`, `focus_harness`/`focused_harness`/`scroll_target_pending`, et l'`EventEmitter<ProvidersEvent>` (plus de focus externe, donc plus aucun événement).
- Tests de nav : `SettingsSection::Agents` → `SettingsSection::Providers`.

- [ ] **Step 7: Vérifier**

Run: `cargo check -p komet-ui --all-targets`
Expected: PASS, aucune référence à `settings::harnesses` (`grep -rn "settings::harnesses\|HarnessesPage" crates/` → vide).

Run: `cargo test -p komet-ui --lib settings::`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add -A crates/ui/src/settings crates/ui/src/settings.rs crates/ui/src/shell.rs
git commit -m "feat(settings): merge agents and accounts into a providers page"
```

---

### Task 4: Cartes dépliables

**Files:**
- Modify: `crates/ui/src/settings/providers.rs`
- Test: `crates/ui/src/settings/providers.rs`

**Interfaces:**
- Consumes: `rows` de la Task 3.
- Produces: champ `expanded: HashSet<HarnessId>`, `fn toggle_expanded(expanded: &mut HashSet<HarnessId>, harness: HarnessId)`, `fn is_expanded(expanded: &HashSet<HarnessId>, harness: HarnessId) -> bool`.

- [ ] **Step 1: Écrire les tests d'expansion**

```rust
#[test]
fn expanding_one_card_leaves_the_others_alone() {
    let mut expanded = HashSet::new();
    toggle_expanded(&mut expanded, HarnessId::ClaudeCode);
    toggle_expanded(&mut expanded, HarnessId::Codex);
    assert!(is_expanded(&expanded, HarnessId::ClaudeCode));
    assert!(is_expanded(&expanded, HarnessId::Codex));
    toggle_expanded(&mut expanded, HarnessId::ClaudeCode);
    assert!(!is_expanded(&expanded, HarnessId::ClaudeCode));
    assert!(is_expanded(&expanded, HarnessId::Codex));
}
```

- [ ] **Step 2: Vérifier l'échec**

Run: `cargo test -p komet-ui --lib settings::providers::tests::expanding_one_card_leaves_the_others_alone`
Expected: FAIL — `toggle_expanded` non définie.

- [ ] **Step 3: Implémenter et brancher le rendu**

`toggle_expanded` : `if !expanded.remove(&harness) { expanded.insert(harness); }`. `is_expanded` : `expanded.contains(&harness)`.

Dans `rows` : l'en-tête devient `.cursor_pointer()` + `.hover(|s| s.bg(crate::theme::ink(0.03)))` + `.on_click(cx.listener(move |this, _, _, cx| { toggle_expanded(&mut this.expanded, harness); cx.notify(); }))`. Ajouter le glyphe `icons::SORT_VERTICAL` en bout d'en-tête comme indicateur de dépliage (seul glyphe disponible ; pas de rotation). Chaque handler enfant (Connect, Install/Update, toggle, Switch, Forget, Add account, Copy sign-in command) commence par `cx.stop_propagation()`.

Le corps n'est rendu que si `is_expanded(&self.expanded, harness)`. La ligne `account_summary` reste toujours visible dans l'en-tête.

- [ ] **Step 4: Vérifier**

Run: `cargo test -p komet-ui --lib settings::providers`
Expected: PASS.

Run: `cargo check -p komet-ui --all-targets`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ui/src/settings/providers.rs
git commit -m "feat(settings): expand provider cards to reveal their accounts"
```

---

### Task 5: Docs, nettoyage et vérification finale

**Files:**
- Modify: `docs/PARITY.md:20` (§1.9)
- Modify: `crates/ui/src/pickers.rs` (commentaire « Settings → Agents »)
- Modify: `crates/ui/src/settings/providers.rs` (copie d'en-tête/subtitle)

**Interfaces:**
- Consumes: tout ce qui précède.
- Produces: plan terminé, docs à jour.

- [ ] **Step 1: Mettre à jour la doc**

`docs/PARITY.md` §1.9 : « Providers settings | done | Une carte par agent (toggle d'activation, état CLI, install/update), comptes multiples par provider (Active/Switch/Forget), flux Add account paste-code et browser-poll, bouton Connect, device switcher (`targetDeviceId`). Pas de jauges d'usage. » Mettre à jour les commentaires qui citent « Settings → Agents ».

- [ ] **Step 2: Vérification complète**

Run: `cargo fmt --all && cargo check --workspace --all-targets && cargo clippy -p komet-ui --all-targets -- -D warnings`
Expected: PASS, aucun warning.

Run: `cargo test -p komet-ui --lib`
Expected: PASS.

- [ ] **Step 3: Vérification visuelle**

Run: `scripts/dev-demo.sh` (ou `KOMET_OPEN_ROUTE=settings/providers cargo run --bin komet`), ouvrir Settings → Providers.
Expected: une carte par agent ; clic sur l'en-tête déplie les comptes ; clic sur toggle/Connect/Install ne déplie pas ; aucune jauge d'usage ; « Add account » ouvre le dialogue d'auth ; plus qu'une seule entrée de sidebar pour les agents.

Vérifier explicitement les cinq points de *Review Focus* : (1) un agent dont le CLI est absent affiche sa carte avec Install et un toggle inerte ; (2) couper l'engine pendant un Refresh montre la bande d'erreur des comptes sans effacer les cartes ; (3) toggle/Connect/Install ne déplient pas ; (4) le dernier agent actif refuse de se désactiver ; (5) avec un device distant sélectionné, le toggle s'applique à ce device (relire `harness_target_device()` dans le code, le test manuel multi-device n'est pas toujours disponible).

- [ ] **Step 4: Commit**

```bash
git add docs/PARITY.md crates/ui/src/pickers.rs crates/ui/src/settings/providers.rs
git commit -m "docs(settings): describe the merged providers page"
```
