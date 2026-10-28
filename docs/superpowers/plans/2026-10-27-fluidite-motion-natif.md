# Fluidité, animations et natif — plan d'implémentation

## Direction

Komet est déjà instrumenté et optimisé par endroits (transcript virtualisé,
watch en deltas, mimalloc, cache de rendu aplati, catalogue de mouvements).
Le travail restant n'est donc pas une réécriture : c'est d'abord **mesurer**,
puis corriger les quelques chemins qui coûtent par frame, puis seulement
ouvrir le chantier animation v2 et le natif Windows.

Règle de conduite : aucune affirmation de gain sans chiffre avant/après issu
des mêmes scénarios instrumentés.

## Ce qui est vérifié

| Constat | Preuve |
|---|---|
| Le survol marque toute la fenêtre sale et relance des frames pendant le fondu. | `hover_listener` appelle `window.refresh()` ([motion.rs:597](../../../crates/ui/src/motion.rs#L597)) ; le pied de rendu du Shell relance une frame tant que `hover_fades_active()` est vrai ([shell.rs:7885](../../../crates/ui/src/shell.rs#L7885)). |
| La même boucle de pulse est écrite deux fois. | `pulse_delta` ([motion.rs:77](../../../crates/ui/src/motion.rs#L77)) et `pulse_lease` ([motion.rs:115](../../../crates/ui/src/motion.rs#L115)) dupliquent le même `spawn`/timer de 16 ms et notifient des vues entières. |
| Le cadencement des loaders est un timer fixe, pas la frame. | `PULSE_TICK = 16 ms` dans [motion.rs](../../../crates/ui/src/motion.rs) ; il peut battre contre le vsync et produire des frames en double. |
| Les fichiers monolithiques restent monolithiques. | `shell.rs` 365 Ko, `composer.rs` 340 Ko, `transcript.rs` 261 Ko, `pickers.rs` 207 Ko, `state.rs` 137 Ko. |
| Une instrumentation existe déjà, contrairement à l'hypothèse « rien n'est mesuré ». | `KOMET_FRAME_STATS` (coût de rendu des lignes live, [transcript.rs:1003](../../../crates/ui/src/transcript.rs#L1003)), `KOMET_NO_RENDER_CACHE` (A/B du cache, [transcript.rs:1017](../../../crates/ui/src/transcript.rs#L1017)), `KOMET_SCROLL_TRACE` ([rail.rs:222](../../../crates/ui/src/rail.rs#L222)), `KOMET_MOTION_SCALE` ([motion.rs:651](../../../crates/ui/src/motion.rs#L651)). `KOMET_GPU_STATS` n'apparaît que dans un commentaire du `Cargo.toml` racine (il appartient au fork gpui), pas dans `crates/`. |
| Il manque la mesure au niveau **frame entière** et le harnais reproductible. | Aucun `scripts/mem-smoke.sh`, aucun scénario scripté ; `scripts/e2e-smoke.sh` est périmé (AGENTS.md). |
| `crates/ui` n'a aucun test d'intégration ni de régression de frames. | Pas de répertoire `crates/ui/tests/` ; seuls des tests unitaires dans `src/`. |
| `PARITY.md` annonçait un gap reduced-motion déjà câblé. | Résolu : la ligne 23 de [PARITY.md](../../PARITY.md) documente que les chemins cœur honorent le flag gpui et qu'il n'existe pas de contrôle Komet dédié. |

À mesurer avant d'agir (hypothèses, pas des faits) : coût réel d'un
`window.refresh()` de survol sur le Shell complet ; nombre de clones par frame
côté transcript ; résidence mémoire des caches et des tuiles GPU.

## Décisions

1. **Mesurer d'abord, une seule fois, avec un harnais jetable mais versionné.**
   On ajoute un budget de frame (p50/p95/p99, frames manquées) et trois
   scénarios scriptés ; on enregistre la baseline dans ce document.
2. **Corriger le survol avant tout refactor.** C'est le gain le plus probable
   et le plus localisé ; il ne demande aucun découpage d'entité.
3. **Unifier l'horloge de pulse** sur la frame plutôt que sur un timer de 16 ms.
4. **Découpage du Shell en entités ensuite seulement**, guidé par ce que la
   mesure désigne, pas par la taille des fichiers.
5. **Animation v2 après la performance.** Scale/rotation et `Tween`/`Spring`
   retargetable supposent un moteur stable et ne doivent pas polluer le
   chemin chaud.
6. **Natif Windows en dernier**, sauf si la mesure montre que la fluidité
   Linux est déjà au budget.

## Phase 0 — Harnais de mesure

- `KOMET_WINDOW_STATS=1` : durée entre deux rendus racine, temps de
  construction de l'arbre dans `render`, compteur de frames manquées. Le
  point d'ancrage existe déjà : le rendu racine unique du Shell. Nom distinct
  de `KOMET_FRAME_STATS` (celui de `transcript`) pour qu'une capture au niveau
  fenêtre n'active pas aussi la mesure par ligne.
  **Limite assumée** : layout, peinture et GPU ne sont pas inclus — gpui ne
  les expose pas à l'app. La promesse initiale d'un « temps de peinture » est
  retirée ; un frame dans son budget de build peut toujours le rater en
  peinture.
- Trois scénarios reproductibles sur moteur mock (`scripts/dev-demo.sh`) :
  1. stream d'une longue réponse ;
  2. scroll d'un transcript de N entrées ;
  3. survol répété de 20 lignes de sidebar, puis ouverture d'un picker.
- `scripts/frame-budget.sh` : lance le binaire **en release** (`[profile.dev]`
  laisse les crates du workspace à `opt-level = 0`, un build debug gonflerait
  le temps de `render` et rendrait le budget de 16,7 ms sans objet ; `--debug`
  reste disponible pour vérifier que l'instrumentation se déclenche),
  agrège les lignes `warn` en p50/p95/p99 et écrit un CSV daté.

Critère d'acceptation : la même commande produit un tableau de chiffres
comparable d'une session à l'autre, et la baseline est recopiée ici.

### État

- Fait : [`crates/ui/src/perf.rs`](../../../crates/ui/src/perf.rs) mesure
  l'intervalle entre passes racine et le temps de construction de l'arbre,
  agrège sur 240 frames et journalise p50/p95/p99 + frames manquées à
  `warn`. Coût nul quand `KOMET_WINDOW_STATS` est absent (un `OnceLock`).
  Branché en tête de `impl Render for Shell`, seul rendu racine de la fenêtre.
- Fait : [`scripts/frame-budget.sh`](../../../scripts/frame-budget.sh) capture
  N secondes, agrège les fenêtres et écrit un CSV ; build release par défaut,
  `--debug` pour le binaire de développement.
- Fait : le script refuse désormais une capture sans intervalle actif, au lieu
  de présenter des percentiles à zéro comme une baseline. Il retire aussi les
  séquences ANSI de `tracing` et exclut les fenêtres inactives des statistiques ;
  vérifié sur les traces réelles et sur des journaux synthétiques.
- Les intervalles hors rafale (> 4 périodes) sont traités comme inactivité, ni
  échantillonnés ni comptés comme manqués : sans cela, une fenêtre au repos
  afficherait des « frames manquées » qui n'en sont pas. Corollaire : une rafale
  trop courte pour remplir les 240 échantillons se vide à la pause dès
  `MIN_FLUSH` (32) échantillons, sinon une capture de 20 s sans animation ne
  produirait aucune ligne.
- Reste à faire : les scénarios scriptés. Un script shell ne peut pas injecter
  de survol ni de molette ; la capture demande donc quelqu'un au clavier
  (survol de la sidebar, scroll d'un transcript, réponse en streaming).
  Automatiser cela demande un pilote de scénario dans l'app, pas un script.
- Baseline : pas encore de chiffres. Elle exige un build complet et une
  session graphique avec interaction ; aucune valeur n'est avancée avant.
- Mesure en session graphique effectuée le : non effectuée ; le script et
  l'instrumentation sont présents, mais aucune interaction automatisée fiable
  n'est disponible dans cette session.
- Essai exploratoire release (20 s, 27-10-2026) : une seule fenêtre agrégée,
  33 intervalles actifs, p50 16,8 ms / p95 32,5 ms, 57,6 % au-dessus de
  16,7 ms ; construction de l'arbre p95 0,7 ms. **Pas une baseline** :
  l'application venait d'être lancée sans session ni transcript, donc cet
  échantillon ne couvre que l'activité de démarrage et ne permet pas de tirer
  de conclusion sur le survol, le scroll ou le streaming.

## Phase 1 — Survol scopé

- Remplacer le `window.refresh()` global par une notification limitée au
  propriétaire du fondu, ou par un état de survol lu au moment du rendu de la
  ligne concernée. La table `HOVER_FADES` étant indexée par clé, il faut
  d'abord associer clé → entité.
- Conserver le fondu là où il porte du sens (lignes de liste, boutons) et
  revenir à un état net sur les lignes denses du transcript si la mesure le
  justifie.

Critère d'acceptation : le listener ne force plus un rafraîchissement global
de la fenêtre ; les vues enfants sont notifiées par leur entité propriétaire.
La mesure de p95 en 1080p reste à faire avant d'affirmer un gain de frame.

### État

- Fait : `hover_listener` reçoit désormais l'entité propriétaire et utilise
  `cx.notify(owner)` ; les appels des popovers, du transcript Markdown, du
  terminal, des pickers, du composer, des panneaux et des contrôles du Shell
  transmettent leur propriétaire. Le filtre des espaces n'appelle plus
  `window.refresh()`. Les fondus encore en vol notifient leur propriétaire à
  chaque frame depuis le pied de rendu du Shell, y compris pour les vues
  enfants dont le rendu est mis en cache.
- Limite : les contrôles appartenant directement à `Shell` invalident encore
  le rendu de cette entité racine. Le découpage du Shell et la baseline
  graphique sont nécessaires pour quantifier le gain ; aucun seuil de p95
  n'est revendiqué ici.

## Phase 2 — Une horloge de pulse

- Garder les API distinctes (`pulse_delta` retourne une phase, `pulse_lease`
  enregistre seulement une vue), mais centraliser le renouvellement des baux.
- Cadencement sur `request_animation_frame` (le pied de rendu racine) plutôt
  que sur un timer de 16 ms, pour éliminer le battement avec le compositeur.
- Notifier l'entité qui peint le spinner, pas la vue hôte entière, quand le
  spinner est isolé (indicateur de travail, pastille de session). Amorcer le
  premier frame au montage d'un spinner, au cas où son rendu a lieu après le
  passage du pilote racine.

Critère d'acceptation : un spinner monté ne provoque qu'un rendu par frame et
zéro frame quand rien ne tourne ; loaders toujours phase-locked.

### État

- Fait : le timer périodique de 16 ms a été retiré. Le Shell demande la frame
  suivante au compositeur seulement tant que des baux sont actifs ; les baux
  expirés sont purgés et les entités enfants actives sont notifiées, tandis que
  le Shell est déjà en rendu. `pulse_delta` et `pulse_lease` partagent leur
  renouvellement, le même registre et la même époque. Une première/ nouvelle
  lease notifie sa vue et demande directement la frame suivante au `Window`
  de rendu, pour amorcer le pilote même quand seule une vue enfant est
  invalidée. Les tests couvrent l'inactivité, l'expiration et le renouvellement
  des baux.
- Fait : la décision reduced-motion est tranchée — `PARITY.md` documente que
  les chemins cœur honorent le flag gpui (`cx.reduce_motion()`) et qu'il
  n'existe pas de contrôle Komet dédié ; aucune tâche de câblage ne reste
  ouverte.
- Reste à mesurer : une capture de session graphique animée est nécessaire
  pour confirmer le rythme réel du compositeur et les frames manquées.

## Phase 3 — Allocations du transcript

- Mesurer d'abord le clone par frame ; s'il est réel, passer les entrées en
  `Arc` et ne cloner que la tranche visible.
- Borner les caches de rendu (LRU à taille déclarée) ; libérer les tuiles GPU
  d'images hors écran, pas à la fermeture.

Critère d'acceptation : RSS stable sur un stream long et sur un scroll répété ;
aucune régression de p95.

## Phase 4 — Découpage guidé par la mesure

Découper `shell.rs` en entités (barre latérale, barre de titre, en-tête,
hôte du panneau droit) une à une, tests verts à chaque étape. Ne pas découper
`composer.rs` et `transcript.rs` avant d'avoir la preuve que leur rendu
complet est le coût dominant.

## Phase 5 — Animation v2

- Vérifier au rev gpui épinglé s'il existe une primitive de transformation ;
  sinon patcher le fork `jomvick/zed` (scale + rotation) comme cela a été fait
  pour EdgeFade et BackdropBlur. Pas de `.rotate(`/`.scale(` dans `crates/ui`
  aujourd'hui : `menu-in`, `dialog-in` et le chevron sont des approximations
  fondu + 2 px.
- Un `Tween`/`Spring` retargetable en vol qui généralise les motifs déjà
  présents (`WidthTween`, `HoverFades`). **Piège connu** : gpui asserte
  `delta ∈ [0,1]` ; un ressort avec dépassement doit passer par le chemin de
  tween manuel, pas `with_animation`.
- Sorties animées au-delà des popovers (dialogues, fermeture d'onglet, toasts,
  insertion/retrait de ligne), puis orchestration (cascade d'entrée,
  transition Chat ↔ Settings).

## Phase 6 — Natif

- Windows : daemon, updater, ACL des credentials, packaging.
- Flou : budget de couches floutées explicite (σ=32 rasterisé par frame via
  wgpu sur Linux), ou flou maison dans `wallpaper.rs` pour un rendu identique
  partout.

## TDD

1. Phase 0 : l'agrégateur de percentiles est couvert par des entrées connues
   (p50/p95/p99) ; la capture graphique reste à faire.
2. Phase 1 : `hover_listener` notifie directement le propriétaire, puis
   retire le rafraîchissement global de la fenêtre.
3. Phase 2 : tests des leases sans état GPUI (aucun actif, expiration,
   renouvellement) ; vérifier en capture que le pilote s'arrête au repos.
4. Phases 3–4 : tests existants en garde-fou, plus un test de borne de cache.
5. Chaque phase se termine par `cargo test -p komet-engine` ciblé si le moteur
   est touché, sinon `cargo test -p komet-ui`, puis
   `cargo check -p komet-ui` et `cargo clippy -p komet-ui -- -D warnings`.

## Risques

- Le `refresh()` de survol n'est peut-être pas le coût dominant : la mesure
  peut désigner le flou, la peinture ou le layout. C'est acceptable — la
  Phase 0 existe pour cela.
- Scoper le survol casse le fondu si une clé est partagée entre plusieurs
  éléments ; le registre clé → entité doit être exhaustif ou échouer bruyamment.
- Patcher le fork gpui impose de suivre la procédure de bump documentée dans
  `AGENTS.md` (fork gpui-component d'abord, puis tous les revs jomvick/zed).

## Non-objectifs

- Pas de refonte du protocole de watch, du ressort de scroll ni du cache
  markdown incrémental : ils fonctionnent.
- Pas de nouveau framework d'animation tiers ; tout reste dans `motion.rs`.
- Pas de promesse de chiffres avant la baseline de la Phase 0.
