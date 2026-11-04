# Widgets Composer : Changes, Sous-agents, Processus — design

## Intention et pourquoi

Trois chips au-dessus du composer (style VS Code Copilot) :
`[Changes] [Sous-agents] [Processus]`. Elles rendent l'état en cours visible
sans ouvrir le panneau droit :

- **Changes** : stats du checkout git. Clic → panneau droit sur Diff (pas de popup).
- **Sous-agents** : liste du tour. Clic → popup ancrée ; clic sur une ligne → onglet SubagentPanel (panneau droit, existant).
- **Processus** : **les commandes que l'agent exécute** (l'ex-panneau droit `AgentShell`). Clic → popup ancrée, lecture seule.

La fusion demandée par l'utilisateur : **« processus » = l'observation des commandes agents** (la donnée existante d'`AgentShell`), PAS un flux de processus détaché. Le panneau droit perd sa surface `AgentShell`.

## Approches considérées

- **A (retenue) — Lecture seule, zéro protocole.** La popup Processus lit `transcript.agent_shell_log()` au paint, comme la surface aujourd'hui. Le §4 du plan d'origine (proto `WatchProcess`/`StopProcess`, buffer moteur, normalisation Claude, `ProcessSurface` + matrice de capacités + golden test) **est supprimé** : ressource ~60 % moindre, zéro changement moteur.
- **B (écartée) — Protocole complet.** Vrais processus avec Stop : inapplicable aux commandes agent (elles tournent dans le CLI, hors de tout PTY Komet — doc « no input affordance »), et double le travail pour le même rendu.

## Règles de visibilité (validées par l'utilisateur)

| Chip | Jamais si | Apparaît quand | Disparaît quand |
|---|---|---|---|
| Changes | toujours là sinon | diff de checkout non vide | 600 ms de temporisation après retour à 0 (hystérésis commit) |
| Sous-agents | `interaction_surface().subagents == None` | ≥ 1 sous-agent du tour | 6 s après le dernier done (sauf popup ouverte) |
| Processus | `interaction_surface().tool_progress == None` | ≥ 1 commande agent tourne | 6 s après la dernière fin ; **reste tant que ≥ 1 est failed** |

Ordre fixe left→right : Changes, Sous-agents, Processus. Un chip masqué ne rend rien. Les trois machines d'état de visibilité vivent dans le strip.

## Appels que je fais (veto possible)

- **Popup Processus en lecture seule** : pas de Stop, pas de bouton Terminate — les commandes tournent dans le shell de l'agent, documenté « never from a terminal Komet controls ». Implique : **pas de §4 Protocole/Moteur/Harnais du plan d'origine** (rpc/proto/engine/capability inchangés).
- **Pas de durée par commande** : `AgentShellEntry` ne porte pas les horodateurs ; la rangée montre état (pastille running pulsante / done / failed), commande tronquée au milieu en monospace, dernière ligne de sortie en `theme.text_muted`. Les durées restent sur les rangées Sous-agents (`3 tools · 12s`, déjà existant).
- **Pas d'entités `ChangesActivity`/`SubagentsActivity`/`ProcessesActivity` isolées** : la dérivation au paint est déjà le motif de la surface actuelle (son coût est valide) ; ajouter trois Entity reviendrait à dupliquer le pipeline `WatchActivity`/diffs. Le strip re-render en descendant d'un Shell notify, comme aujourd'hui.
- **Le chip transcript « voir les commandes » (TranscriptEvent::OpenAgentShell) est retargeté** : ouvre la popup Processus au-dessus du composer au lieu d'une surface de panneau droit.
- **Réutilisation stricte** : pill générique inspirée de `pickers.rs::footer_chip`, popups via `popover.rs::anchored_menu_above*` + `Popup<WidgetKind>` (motif `Popup<PickerKind>` / `user_menu` : deux instances différentes cohabitent déjà).

## Architecture

### 1. `crates/ui/src/composer_widgets.rs` [NEW — un seul module]

- `enum WidgetKind { Subagents, Processes }` + `Popup<WidgetKind>` partagé :
  `press-note` (`note_trigger_press_matching`) sur un mouse-down des chips, toggle au click, `on_mouse_down_out` → close — même squelette que `pickers.rs`.
- Accorder sa barre : conteneur absolu au-dessus de la zone composer, `gap(4)` ; rendu séquentiel Changes/Sous-agents/Processus, chacun `Option<AnyElement>`.
- Pill générique : `h 24` `rounded_full` bordure `theme.border`, fond `ink(0.04) → 0.07 hover → 0.10 open`, texte 11,5, padding h 9, chevron bas pour les deux widgets à popup.
- Pure et testée sans gpui :
  - `summarize_patch(files) -> Option<(usize files, u32 add, u32 del)>` (à partir du résumé du ChangesPanel existant, `changes.rs` fields `additions`/`deletions`) ;
  - `format_count(u32) -> String` (≤999 tel quel, ≥1000 `1.2k`) ;
  - Visibility state-machines : hystérésis 600 ms (rafraîchie par le ticker existant du Shell) et délai done→6 s — pure, `fn visible(bool, Option<Instant>) -> bool`.
- Rangées popup Processus : pastille d'état + commande tronquée au milieu (monospace) + dernière ligne de sortie, depuis `AgentShellEntry` (le module référence `transcript::{AgentShellEntry, AgentShellStatus}`, pub).
- Rangées popup Sous-agents : pastille, titre, modèle, `3 tools · 12s` (reprend `subagent_status_label`), open tab au click → ferme la popup.
- Ancrage : card popup 340 px, max-h 320, `anchored_menu_above` + garde anti-clip 8 px du bord de fenêtre, `menu_in`/`menu_out` existants, `on_mouse_down_out` → close, Échap via focus du card.

### 2. `crates/ui/src/shell.rs` + `shell/content.rs` + `shell/right_pane.rs` [MODIFY]

- Strip instancié par **Shell** (il possède `transcript`, `diffs`, le panneau droit), rendu enfant flex de la colonne `content.rs` directement au-dessus de `self.composer` (ligne 156) — pas de positionnement absolu nécessaire.
- `RightSurface::AgentShell` supprimé partout : la variante (navigation.rs:93), `add_agent_shell_surface` (right_pane.rs:757), l'onglet + l'icône dans `right_pane.rs` (liste 1119-1122), le match de surface (content.rs:794-817), et **la carte « Shell agent » du surface picker** (content.rs:1011-1015 — chemin mort sinon). `agent_shell.rs` (le layout panneau) est absorbé : ses helpers repris par la popup migrent dans `composer_widgets` ; `agent_shell_entries` et ses tests purs **restent** dans `transcript.rs` (tests à partir de 6886). Aucune persistance ni aucun test existant n'utilise la variante (shell/tests.rs 576-630 ; `right_tabs`/`ChatPanels` in-memory only).
- Handlers : `open_changes_widget(cx)` idempotent (pas de toggle) ; popup sous-agents → `add_subagent_surface(open, cx)` existant ; popup processus ferme au clic sur une rangée (lecture seule).
- Chip transcript : `TranscriptEvent::OpenAgentShell` (shell.rs:513) → **ouvre la popup Processus du chat courant** : l'événement est émis depuis le transcript du chat sélectionné, aucune donnée à ajouter. Le strip est chat-contextuel — les trois chips décrivent le chat sélectionné.
- Horloge des machines d'état : dès qu'un chip a une deadline (600 ms ou fin de 6 s), le strip programme un re-notify one-shot à l'échéance (motif `provider_switch_task`/`reap_popup`), recalculé à chaque transition — pas une horloge par chip ; le ticker global du Shell ne suffit pas (il ne notifie que sur live indicator).

### 3. Données (aucun nouveau flux réseau)

- Changes : **le résumé ne vient PAS de `self.diffs`** (les entités `Entity<Changes>` ne vivent que si un onglet Diff a été ouvert — `add_diff_surface` right_pane.rs:668, watch idempotent `ensure_watch` changes.rs:1096). Le strip garde un chemin propre : une entité `Changes` headless dédiée au checkout du chat courant, créée lazy à la première frame du strip, dont `add_diff_surface` **réutilise** l'entité si elle existe (create-or-reuse, un seul watch par checkout). Summary = pure fn sur les `FileDiff` du set (`additions`/`deletions`, changes.rs 172-173).
- Sous-agents : `subagent_live` est privé dans `Transcript` — ajouter un accesseur pub + une derivation pure (`rows → rangées`) à côté d'`agent_shell_entries`, testée pareil.
- Processus : `transcript.agent_shell_log()` (pur, déjà testé — transcript.rs:6886+).

## Non-goals

- Aucun Stop/durée sur les commandes agent ; aucune écriture, jamais, vers le shell de l'agent.
- Pas de proto/rpc/engine/harness : §4 d'origine retiré ; `crates/harness` et `crates/engine` non touchés ; la matrice de capacités est déjà **suffisante** (`subagents`, `tool_progress`).
- Pas de paginement dans les popups (listes bornées, les plus récentes en tête).

## Plan de vérification

```bash
cargo test -p komet-ui composer_widgets        # summarize_patch / format_count / hystérésis 600 ms / 6 s done
cargo test -p komet-ui                         # régression : agent_shell_log, surfaces du panneau droit, tests shell (AgentShell retiré)
cargo check -p komet-ui && cargo clippy -p komet-ui --all-targets -- -D warnings
```

Le reste suit la vérification manuelle du plan d'origine, traduite : Changes ouvre Diff sans toggle ; popup sous-agents → onglet ; popup processus lecture seule ; hystérésis au commit ; et un harness qui déclare `subagents: none` ou `tool_progress: none` ne montre **jamais** le chip correspondant.
