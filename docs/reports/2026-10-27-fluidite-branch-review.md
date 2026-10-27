# Revue de la branche « fluidité / motion natif » (PR #67)

Date : 2026-10-27
Branche : `freebuff/j-ai-lu-architecture-md-agents-md-parity-md-memory-c8115dbe-5215-42d5-98ac-780e13746a96`
Base : `origin/main` (`0dd4460`)
Périmètre : 26 fichiers, +1574 / −570 ; `crates/ui/src/{motion,perf,shell,composer,transcript,…}.rs`

## Verdict

Le socle est propre et sans régression fonctionnelle détectée : le timer de 16 ms
disparaît au profit du vsync, la première lease amorce explicitement une frame, et
le survol n'appelle plus `window.refresh()`. En revanche, **l'objectif de perf
n'est pas encore atteint**, pour deux raisons structurelles détaillées ci-dessous.
La PR est mergeable en tant que prérequis, mais aucun gain chiffré ne doit être
revendiqué avant les phases A et B.

## Vérifications effectuées

- `cargo check -p komet-ui --all-targets` : OK.
- `cargo clippy -p komet-ui --all-targets -- -D warnings` : OK.
- `cargo test -p komet-ui` : **586 tests passent** (dont les nouveaux tests de
  leases, d'expiration et de propriétaires de fade).
- `cargo fmt --all -- --check` et `git diff --check` : OK.
- Relecture intégrale du diff et du code gpui épinglé
  (`jomvick/zed @ e2ddcc6`, `crates/gpui/src/{view,window,app}.rs`).
- **Non effectué** : capture graphique réelle. La session était occupée (console
  Qoder au premier plan) ; lancer komet en plein écran avec injection `ydotool`
  aurait volé le focus et cliqué dans les fenêtres de l'utilisateur. C'est aussi
  la mesure manquante du plan Phase 0.

## Ce qui fonctionne bien

- Suppression du timer périodique : plus de battement 62,5 Hz vs 60 Hz.
- `request_animation_frame()` à la première lease : vérifié contre gpui —
  `request_animation_frame` notifie la vue courante à la frame suivante, et
  `App::notify` est filtré par `tracked_entities` (pas de panique sur une entité
  démontée). Cela corrige le gel possible après une invalidation enfant seule.
- La racine est exclue des notifications de pulse et de hover ; rien ne tourne au
  repos ; `cx.reduce_motion()` est respecté (loaders, hover, `WidthTween`,
  `flip_morph`, glide du rail).
- Threading `owner`/`window` mécanique et exhaustif (compilé et testé) ; bouton
  copier du markdown rendu optionnel via `hover_owner`.
- Tests unitaires pertinents (inactivité, expiration, renouvellement, propriétaires
  distincts).

## Constats

| # | Sévérité | Constat | Référence |
|---|---|---|---|
| 1 | Haute | **Le scoping d'invalidation est inerte sans cache de vue gpui.** Aucun `.cached(...)` dans `crates/ui` ; un `ViewElement` sans `cached_style` re-rend son entité à chaque construction par le parent, et la racine Shell n'est jamais cachée. Chaque frame dessinée re-rend donc Shell + Composer + Transcript + Changes. `cx.notify(owner)` coûte à peu près comme l'ancien `window.refresh()` ; les notifications enfants de `pulse_frame` sont redondantes ; le découpage du Shell (Phase 4 du plan) ne servira à rien sans cache. | `shell.rs:5879`, `shell.rs:5974`, `shell.rs:922` ; gpui `view.rs` (`request_layout`, `cached_style`) |
| 2 | Haute | **L'instrumentation sous-mesure.** `perf::frame()` est lâché à la fin de `Shell::render`, mais les rendus enfants ont lieu plus tard, pendant `request_layout` des `ViewElement` dans `draw_roots`. `build_p95 = 0,7 ms` ne couvre que le corps du Shell, pas Composer/Transcript/layout/paint — incohérent avec 57,6 % de frames manquées. La doc de `perf.rs` parle pourtant de « whole window ». | `perf.rs:9-11`, `shell.rs:7874` |
| 3 | Moyenne | Un spinner de session (sidebar) appartient à **Shell** : tant qu'un agent tourne, l'application entière re-rend à ~60 fps, y compris les rangées hors écran → CPU/batterie. | `shell.rs:4327`, `shell.rs:6404`, `shell/spaces.rs` |
| 4 | Moyenne | Artefacts de mesure : budget figé 60 Hz ; fenêtre inactive plafonnée à 30 fps par gpui → comptée comme « manquée » ; paint et GPU non mesurés. | `perf.rs:42` ; gpui `window.rs` (throttle 33 ms) |
| 5 | Basse | Allocations par frame : `Vec` dans `active_views`, `Vec` + tri dans `active_owners` ; requêtes de frame redondantes (paint de l'orb + amorçage de bail + pied de rendu). | `motion.rs:73`, `motion.rs:566`, `thinking_orbs.rs:1123` |
| 6 | Basse | `frame-budget.sh` exige un démon, mais le seul lanceur (`dev-demo.sh`) ouvre le GUI debug : friction pour une capture release. | `frame-budget.sh:52`, `dev-demo.sh` |
| 7 | Info | Les clés de hover restent des chaînes globales ; le risque « clé partagée entre deux éléments » est documenté mais sans garde-fou. | `motion.rs:601` |
| 8 | Info | L'historique de la PR contient la paire `c5d1626` (ajout) / `21830db` (revert WIP) qui s'annule ; sans impact sur le diff final, mais bruit d'historique. | `git log` |

## Plan proposé

### A. Terminer la mesure (S, ~1 j) — prérequis

- Instrumenter aussi les rendus enfants (garde dans `Render for Transcript`,
  `Composer`, `Changes`) et agréger un `render_total` par frame.
- Corriger la doc de `perf.rs` (ce qui est mesuré / ne l'est pas) et le cas
  « fenêtre inactive à 30 fps ».
- Ajouter un mode démon seul (`dev-demo.sh --daemon-only` ou
  `scripts/demo-daemon.sh`) pour des captures release reproductibles.
- Enregistrer une vraie baseline pour trois scénarios : survol de la sidebar,
  réponse en streaming, scroll d'un transcript.

Critère d'acceptation : une commande, chiffres comparables d'une session à
l'autre, part « Shell seul » vs « enfants » visible.

### B. Rendre le scoping réel : activer le cache de vue gpui (M, 2–4 j) — le vrai gain

- Spikes `.cached(style)` sur **une** région (Changes ou sidebar), puis Composer,
  Transcript, Terminal. L'API du fork existe et son commentaire cite le composer.
- Auditer le contrat d'invalidation : chaque donnée lue par la vue cachée doit
  notifier cette entité (State, input, scroll, thème, focus/IME). Ajouter des
  tests anti-périmé (changement de chat, delta de stream, thème, focus).
- Le plumbing `owner` de cette PR devient alors utile : un survol de sidebar ne
  re-rend que Shell + Sidebar ; un spinner de Transcript que Shell + Transcript.

Critère d'acceptation : survol sidebar sans rendu de Transcript/Composer
(compteurs), p95 ≤ budget sur les trois scénarios, zéro frame au repos, aucune UI
figée.

### C. Découper le Shell (M, 2–3 j) — après B

- Extraire Sidebar, Titlebar et hôte du panneau droit en entités cachées ; garder
  le `render` racine minuscule. Sans cache, ce découpage ne change rien.

Critère d'acceptation : tests verts à chaque étape ; budget stable ou meilleur.

### D. Unifier les horloges restantes (S/M)

- Le glide du rail utilise encore une boucle `timer(16 ms)` (`rail.rs:262`) : le
  passer sur le même pilote frame-driven.
- Mettre en pause les leases des indicateurs hors écran et, si besoin, plafonner
  les mini-orbs (14 px) à 30 fps.

Critère d'acceptation : plus aucun timer dans le chemin d'animation ; idle = 0
frame ; spinner = 1 rendu/frame.

### E. Allocations du transcript (M)

- Mesurer le clone par frame ; passer les payloads de lignes en `Arc` et ne
  cloner que la tranche visible ; borner `RenderCache` (LRU) et libérer les
  tuiles GPU hors écran à l'éviction.

Critère d'acceptation : RSS stable sur stream long + scroll répété, sans
régression p95.

### F. Animation v2 et cohérence (L, parallélisable)

- Scale/rotation (patch fork selon la procédure AGENTS), `menu-in`/`dialog-in`/
  chevron fidèles ; sorties animées (dialogues, fermeture d'onglet, toasts) ;
  `Tween`/`Spring` retargetable généralisant `WidthTween` + `HoverFades` ;
  matrice reduced-motion.

Critère d'acceptation : catalogue appliqué de façon homogène, aucune régression
de budget.

### G. Chasse aux bugs et cohérence

- Assert debug : une clé de hover ne peut pas changer de propriétaire sans
  `set_hover`.
- Tests de non-régression du cache (B) et du cycle bail/frame.
- Documenter l'artefact 30 fps en fenêtre inactive.

## Risques

- **Cache de vue non sûr** si une vue lit une donnée sans être notifiée → UI
  figée. Mitigation : spike sur une région, tests ciblés, compteurs de rendu.
- Le layout d'une vue cachée doit être fourni ; les régions à bounds changeants
  doivent invalider leur cache.
- Patcher le fork pour les transformations impose la procédure de bump d'AGENTS
  (fork gpui-component d'abord, puis tous les revs jomvick/zed).
- Ne pas perdre l'amorçage de frame à la première lease en refactorant.

## Recommandation

Merger la PR #67 comme socle (sûr, utile, tests verts) sans revendiquer de gain
de perf. Enchaîner immédiatement sur **A puis B** : c'est là que se trouvent les
gains réels, et B exploite enfin le cache de vue fourni par le fork sans réécrire
l'architecture.

## État d'exécution (2026-10-27)

- **A — fait.** L'échantillon est finalisé au début de la frame suivante : les
  rendus enfants ont lieu après le retour de `Shell::render`, dans le prepaint
  des `ViewElement`. La table sépare `build` / `children` / `total` et compte
  les rendus par vue (`transcript_renders`, `composer_renders`,
  `changes_renders`) ; la fenêtre inactive n'est plus notée (throttle ~30 fps).
  `frame-budget.sh` lit ces champs (table + CSV) et `dev-demo.sh --daemon-only`
  fournit un démon seul pour les captures release.
  *Reste à faire : la baseline chiffrée.* La session est Wayland active sans
  compositeur headless (`Xvfb`/`weston`/`cage`/`sway` absents) ; lancer l'appli
  et injecter `ydotool` volerait le focus du bureau. À faire par un humain avec
  `scripts/dev-demo.sh --daemon-only` puis `scripts/frame-budget.sh`.
- **B — fait pour Transcript et Changes.** Les deux vues sont montées en
  `Entity::cached(StyleRefinement::default().size_full())`. Le contrat
  d'invalidation est celui de gpui : `window.mark_view_dirty` remonte aux
  ancêtres, donc un `cx.notify` d'une vue embarquée (historique, bouton fetch,
  recherche de branche, éditeur de prompt en place, curseur) invalide bien le
  cache parent ; les changements de thème passent par `refresh_windows`, et les
  bornes font partie de la clé de cache. **Composer différé** : sa hauteur est
  mesurée au contenu et le flip morph suppose un layout défini, ce que le cache
  interdit ; il demande un conteneur à hauteur pilotée avant d'être caché.
  Aucune validation à l'écran n'a été faite (même contrainte que ci-dessus).
- **G — partiel.** Garde debug ajoutée dans `HoverFades::set_at` : une clé de
  hover encore survolée par une autre vue déclenche une assertion (une clé en
  cours de sortie peut changer de main), avec tests. L'artefact 30 fps est
  documenté dans `perf.rs` et `frame-budget.sh`. Les tests de non-régression du
  cache viendront avec B validé à l'écran.
- **C, D, E, F — non entamés.** C dépend d'un B vérifié dans l'appli (découper
  sans cache ne change rien) ; D est une réécriture du glide du rail qu'on ne
  peut pas vérifier sans session graphique ; E attend une mesure fiable ; F
  impose la procédure de bump du fork décrite dans AGENTS.md.
- Validation locale : `cargo fmt --all`, `cargo check -p komet-ui --all-targets`,
  `cargo clippy -p komet-ui --all-targets -- -D warnings`, `cargo test -p
  komet-ui` (591 tests), `bash -n` sur les deux scripts — tout vert.

