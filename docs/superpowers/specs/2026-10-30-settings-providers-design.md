# Settings → Providers : fusion Agents + Accounts

Date : 2026-10-30

## Intention et pourquoi

Les réglages d'agents sont coupés en deux pages qui parlent du même objet :

- **Agents** (`settings/harnesses.rs`) : une carte par harness, toggle
  d'activation, état d'installation du CLI, bouton « Connect » qui ne fait
  qu'ouvrir l'autre page.
- **Accounts** (`settings/accounts.rs`) : une section par provider, lignes de
  comptes (Active, Switch, Forget), jauges d'usage, flux « Add account »
  (paste-code / browser-poll), et, pour les providers sans login in-app, la
  commande CLI à copier.

L'utilisateur doit aujourd'hui faire des allers-retours entre les deux pour une
seule décision : « cet agent, je l'active, et je me connecte à quel compte ? ».
On fusionne en une page **Providers** : une carte par agent, cliquable, qui
déplie ses comptes et son bouton d'authentification. Le toggle d'activation
reste sur la carte. Les jauges d'usage disparaissent.

## Objectifs

- Une seule entrée de sidebar « Providers », qui remplace « Agents » et
  « Accounts ».
- **Tous** les agents apparaissent, y compris ceux sans notion de compte
  (Pi, Opencode, Hermes, Mock en dev) : carte avec toggle, corps déplié qui
  explique l'auth CLI quand il n'y a pas de login in-app.
- Chaque carte garde : toggle d'activation, état CLI (installé / version /
  mise à jour / bouton Install-Update), bouton « Connect » quand l'agent
  requiert un login et n'est pas connecté.
- Corps déplié : comptes enregistrés (Active, Switch, Forget), « Add account »
  quand un login in-app existe, sinon l'instruction + « Copy sign-in command »,
  plus les warnings du provider.
- Les boutons « Add account » et « Connect » lancent tous deux le vrai flux
  d'auth (`StartAgentLogin` → paste-code ou browser-poll) : ils gardent leur
  rôle d'authentification, aucun ne se contente de naviguer.
- Aucune jauge d'usage, aucune sonde d'usage.

## Non-objectifs

- Pas de changement proto/engine : `ListHarnesses`, `GetHarnessCliStatus`,
  `SetHarnessEnabled`, `ListAgentAccounts`, `Start/Complete/Poll/CancelAgentLogin`,
  `ActivateAgentAccount`, `ForgetAgentAccount` restent tels quels.
- Pas de gestion de comptes multi-device nouvelle : le device switcher existant
  reste, un seul pour toute la page.
- Pas de refonte du picker de modèles ni du composer.

## Anti-objectifs

- Une page qui n'affiche que les providers ayant des comptes (contredit la
  demande : tous les agents).
- Deux entités empilées sur une route commune (deux chargements, deux
  switchers, deux états d'erreur) : ce n'est pas une fusion.
- Garder les jauges « juste masquées » : la tuyauterie d'usage doit partir.

## Contraintes

- gpui : un clic sur un enfant remonte au parent sauf `cx.stop_propagation()`.
  Le toggle et les boutons d'en-tête doivent l'appeler, sinon ils déplient la
  carte en même temps.
- gpui clé l'état interactif par chemin d'id : les ids des lignes de compte
  restent basés sur l'id de compte (déjà le cas), et les ids des cartes sur
  `HarnessId`.
- L'état d'activation est **par device** (`harness-prefs.json`) ; les comptes
  aussi. Le device switcher doit retarger les deux chargements.
- Les tests UI sont des tests unitaires purs dans `crates/ui` ; la page se
  vérifie avec `cargo check -p komet-ui` + `cargo test -p komet-ui settings::`.
- `Mock` ne s'affiche qu'avec `KOMET_HARNESS=mock` (via
  `pickers::visible_harnesses`) — on réutilise ce filtre, pas un nouveau.

## Approches

- **A. Page unique fusionnée, cartes dépliables** *(choisie)* :
  `settings/accounts.rs` devient `settings/providers.rs`, absorbe
  `harnesses.rs`, une seule entité, un seul chargement (harnesses + CLI status +
  comptes), cartes dépliables. `HarnessesPage` supprimé, une seule section de
  sidebar, les anciennes routes redirigent.
- **B. Même fusion, comptes toujours visibles** : moins de code (pas d'état
  déplié) mais page longue et toggle noyé au milieu des comptes.
- **C. Garder les deux pages + un raccourci « Providers »** : le moins de code,
  mais pas de cartes cliquables.

## Décisions (déjà tranchées)

- Tous les agents en carte, pas seulement ceux qui ont un compte.
- Pas de distinction « avec/sans login in-app » dans la structure : la carte a
  un bouton d'auth quand elle peut en avoir un, sinon le texte CLI.
- Pas de jauges d'usage : suppression, pas masquage.

## La conception

### Fichiers

- `crates/ui/src/settings/accounts.rs` → `git mv` vers
  `crates/ui/src/settings/providers.rs`, puis :
  - `AccountsPage` → `ProvidersPage`, `AccountsEvent` → `ProvidersEvent`.
  - Absorbe depuis `harnesses.rs` : `blurb`, `cli_name`,
    `AgentConnectionStatus`, `show_connect_button`, `newly_available_cli_update`,
    l'état CLI (`cli_status`, `cli_update_prompt`, `announced_cli_updates`,
    `managing_cli`, `checking_updates`), `toggle`, `manage_cli`,
    `render_cli_update_dialog`, `check_updates`, le chargement
    `ListHarnesses`/`GetHarnessCliStatus`/`SetHarnessEnabled`.
  - Supprime la tuyauterie d'usage : `UsageLevel`, `usage_level`,
    `usage_color`, `LoadTrigger`, `usage_mode_for`, `format_reset`,
    `displayed_usage`, `supports_usage_probe`, `retain_previous_usage`,
    `render_usage_meter`, les fantômes de jauges des skeletons, et leurs tests.
    Les appels `ListAgentAccounts` passent `usageMode: "none"`.
- `crates/ui/src/settings/harnesses.rs` : supprimé (ses helpers purs ont migré).
- `crates/ui/src/shell.rs` : une seule section `SettingsSection::Providers`,
  label « Providers », icône `WIDGET` ; suppression de `Harnesses` et `Agents`
  (variantes, labels, icônes, `ALL`) ; un seul champ `providers_page` ;
  routes `settings/providers` (+ alias `settings/agents` et
  `settings/harnesses`) ; `open_accounts_for_harness` →
  `open_provider_for_harness` qui pose `pending_provider_harness` puis ouvre
  Providers.
- `docs/PARITY.md` §1.9 : réécrit pour décrire la page fusionnée sans usage.
- `crates/ui/src/composer.rs` / `pickers.rs` / `transcript.rs` : seul
  `harnesses::cli_name` est référencé ailleurs (composer) — il migre vers
  `providers::cli_name` et l'import change. `pickers.rs` mentionne « Settings →
  Agents » dans un commentaire : mis à jour.

### Chargement

Au montage de la page (recréée à chaque visite, comme l'ancienne page Agents,
pour re-sonder les CLI) :

1. `ListHarnesses` (target device) → `Vec<HarnessDescriptor>`, filtré par
   `visible_harnesses` : c'est **la liste des cartes**, dans cet ordre.
2. `GetHarnessCliStatus` (background, `checkLatest: false`) → versions.
3. `ListAgentAccounts` avec `usageMode: "none"` → comptes, joints par harness.

Un seul `refreshing` couvre 2 et 3 ; un seul `error` affiche l'échec du
chargement principal (les échecs de comptes s'affichent dans la carte du
provider concerné, comme aujourd'hui via les warnings du snapshot).

### Carte (repliée)

En-tête cliquable (curseur pointer, hover léger) :

`marque · nom · blurb` … `[Connect] [toggle] [chevron]`

- Le clic sur l'en-tête bascule l'expansion (`HashSet<HarnessId>` ; plusieurs
  cartes peuvent être ouvertes).
- `Connect` : visible selon la règle existante `show_connect_button`
  (login requis, pas connecté, login in-app existant) ; clic →
  `start_login(harness)` + `cx.stop_propagation()`.
- Toggle : même règle qu'aujourd'hui (désactivé si CLI absent ou si c'est le
  dernier agent actif) + `cx.stop_propagation()`.
- Ligne d'état sous le blurb quand replié : « N accounts · Active: email »
  ou « Not connected » / « Install the X CLI to enable » / version / mise à
  jour disponible. (Ces lignes existent déjà, on les garde.)
- Bouton `Install` / `Update` (CLI managé) : reste dans l'en-tête, avec
  `stop_propagation`.

### Carte (dépliée)

Sous l'en-tête, dans le même conteneur (pas de carte séparée) :

- Warnings du provider (une bande par warning).
- Lignes de comptes : avatar initiale, email, badge Active, plan, `Switch` /
  `Forget` — **sans** jauges ni « Usage unavailable ». Seul le compte actif
  reste en tête (tri existant).
- Pied de corps :
  - login in-app supporté → bouton « Add account » (`start_login`).
  - sinon → texte d'instruction + « Copy sign-in command » (commandes
    existantes), et pour un provider sans aucune notion de login (Mock) rien
    de plus que les comptes (vides).
- Aucun compte + login in-app → l'état vide actuel (« No X login detected » +
  bouton Add account).

### Auth

Les deux entrées (`Connect` et `Add account`) appellent le même
`start_login`, donc le même dialogue (`render_login_dialog`, inchangé) :
paste-code pour Claude/Antigravity, browser-poll pour Codex et les autres.
Après succès, rechargement des comptes (`usageMode: "none"`).

### Focus depuis l'extérieur

La puce d'erreur d'auth du transcript (`ErrorOpenAccounts`) n'a pas de harness
dans son événement (`RowKind::ErrorChip` et `AgentEvent::Error` n'en portent
pas, et on ne touche pas au proto) : elle ouvre Providers, sans dépliage ni
scroll. C'est exactement ce qu'elle fait aujourd'hui (elle ouvre Accounts sans
focus), donc pas de régression. Conséquence : `pending_provider_harness`,
`ProvidersEvent::FocusProvider` et l'état `focused_harness` disparaissent au
lieu d'être portés.

### Calls du builder (tranchés pendant la revue du doc)

1. **Échec de `ListAgentAccounts`** : les comptes gardent leur propre
   `Loadable<AgentAccountsSnapshot>` et leur propre bande d'erreur au niveau de
   la page (comme aujourd'hui), indépendante du `Loadable` des harnesses. Un
   échec des comptes n'efface pas les cartes ; les cartes s'affichent avec
   « comptes indisponibles » dans le corps, sans inventer « non connecté ».
2. **Ciblage device** : un seul `target_device: Option<String>`, et
   `with_target` résout `self.target_device.clone().or_else(||
   state.harness_target_device())` **à chaque RPC** (sémantique de l'ancienne
   page Agents), pour que toggle, Switch/Forget et login écrivent sur la même
   machine que celle dont la liste est affichée.
3. **Mise à jour des CLI** : « Check for updates » reste un contrôle séparé
   (à côté de Refresh), seul chemin de `checkLatest: true, force: true` ; le
   dialogue de mise à jour et `announced_cli_updates` restent donc vivants.
   Refresh recharge harnesses + CLI (`checkLatest: false`) + comptes.
4. **Ordre des cartes** : celui de `ListHarnesses` filtré par
   `visible_harnesses` ; un harness sans compte a quand même sa carte.

### Ce qui est laissé au builder

- Découpage exact des fonctions de rendu (une `render_card`, une
  `render_card_body`, un `render_account_row`), noms des champs d'état.
- Choix des ids d'éléments gpui pour les cartes et le chevron.
- Ordre visuel exact des actions d'en-tête, espacements, tailles — suivre
  `widgets.rs` et le style des cartes existantes.
- Faut-il garder `PROVIDERS` (const des 13 harnesses + noms) : oui pour les
  noms/commandes CLI, mais la liste des cartes vient de `ListHarnesses`.
- Stratégie de test : tests purs sur le join (chaque descriptor produit une
  carte même sans compte), sur `show_connect_button`, sur la bascule
  d'expansion (fonction pure `toggle_expanded`), et sur les helpers de login
  conservés.
