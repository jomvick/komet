# Transparence et interaction agent — compaction, sous-agents, questions, connexion

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development
> (recommended) or superpowers:executing-plans to implement this plan task-by-task.
> Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Rendre Komet plus interactif et la connexion agent ↔ Komet beaucoup
plus fluide. L'UI montre tout ce que le CLI de base montre — actions, sorties,
compaction, sous-agents, questions et décisions — au moment où cela se produit,
et permet d'agir (envoyer, steer, arrêter, répondre, approuver, relancer) sans
attendre ni deviner l'état de la connexion. Persistance, non-blocage et
honnêteté sur les capacités restent les contraintes.

**Architecture:** Un `AgentEvent` canonique par fait observable, normalisé dans
l'adaptateur du harness ; l'engine route et persiste (doc parent avec
métadonnées bornées, entrées `System`, projection à la demande du journal de
run pour les sous-agents, canal éphémère `WatchActivity`) ; l'UI rend une seule
famille de lignes, une seule file de demandes et un seul état de connexion
(moteur, harness, modèle). La matrice d'interaction déclarée par harness
empêche toute affordance impossible ; les animations réutilisent
`motion.rs`/`thinking_orbs.rs`, jamais une boucle ad hoc.

**Tech Stack:** Rust edition 2024, serde additif, Loro (doc CRDT), gpui
(jomvick/zed), ACP v1, Claude stream-json, Codex app-server, Cursor SDK shim,
Muse MSP, Antigravity NDJSON.

**Spec:** Demande utilisateur 2026-10-28 — « rendre Komet plus interactif et la
connexion agent Komet beaucoup plus fluide ; cela doit toucher l'UI et l'UX ».
Complète la demande de transparence du 2026-10-27 et
`2026-08-26-permission-ui.md` (ne les remplace pas) ; s'appuie sur
l'instrumentation de `2026-10-27-fluidite-motion-natif.md`, désormais dans
`main` avec la PR #68 (commit `09b04ed`).

**Branche:** créer `feat/agent-transparency` depuis `main`. Deux gates
explicites :

- **Gate A — fluidité (ouverte).** PR #68 est mergée dans `main` (commit
  `09b04ed`) : `crates/ui/src/perf.rs` et `scripts/frame-budget.sh` y sont
  désormais. Task 8 n'a plus de blocage ; son Step 0 étend les compteurs
  existants (`ContextUsage`, `Rail`, rendu par ligne) avant toute mesure.
- **Gate B — ACP.** Les RFD *Session Compaction* et *Elicitation* sont des
  propositions de protocole, pas des standards figés. L'annonce des capacités
  reste derrière un réglage désactivé par défaut et n'est activée qu'après
  validation sur un agent réel (Task 0, Step 2b).

**Révision v2 (revue du 2026-10-27) :** dépendance fluidité levée par le merge
de #68, compteurs par vue à étendre ; RFD ACP traitées comme brouillons derrière un
réglage ; les agents ACP fusionnés en un chemin partagé ; sous-agents servis
par le journal de run au lieu d'un doc enfant ; compteur d'outils enfant écrit
seulement à l'état terminal ; I2 réduit à ce qui est réellement durable ;
tie-break des réponses défini par l'ordre du doc ; résumé de compaction
journal-only par défaut avec export/sync explicites.

**Révision v3 (analyse du code, 2026-10-28) :** Qoder ajouté (14 harnesses,
8 agents ACP) ; journal traité comme ressource du chat (discard à la
suppression, rétention) ; événements éphémères explicitement hors journal ;
writer unique et idempotent des réponses ; `source` porté par
`InputRequested` ; raison d'échec de compaction et `tokens_after` définis ;
id de compaction par compaction (pas par run) ; `export.rs` ajouté au
périmètre ; compteurs perf et `scripts/frame-budget.sh` alignés ; séquencement
des PRs corrigé et recommandations de revue en §8 ; I6 (auto-allow) doté d'une
tâche ; contrat `GetToolDetail` clarifié ; durée de sous-agent déclarée non
reconstructible après crash.

**Révision v4 (objectif produit, 2026-10-28) :** l'objectif inclut
l'interactivité et la fluidité de la connexion agent ↔ Komet. Ajout de deux
tâches UX (connexion lisible avec backoff et reprise ; interaction directe au
clavier et actions inline) et de deux invariants I7/I8. La connexion distingue
le lien moteur, le démarrage du harness et l'attente du modèle ; aucun écran
blanc pendant une reconnexion ; aucune animation ad hoc — réutilisation de
`motion.rs` et `thinking_orbs.rs`. L'ancienne Task 10 (observabilité/docs)
devient Task 12.

**Révision v5 (exhaustivité, 2026-10-28) :** « rien n'est ignoré » devient un
invariant (I9) et un inventaire revu en §0.1 — chaque fait observable a une
destination explicite (doc, journal, éphémère ou `None` déclaré). La carte Plan
devient obligatoire dans Task 7 (dérivation `latest_plan` + cache UI + carte
au-dessus du composer) ; les tâches et les sous-agents ne sont plus des
affordances optionnelles.

**État d'implémentation (2026-10-29) :**
- **Task 0 livrée** (hors Step 2b, validation par agent) :
  `komet_proto::HarnessInteractionSurface` + 5 enums ; table exhaustive
  `komet_harness::capabilities::interaction_surface` (14 lignes, pas de `_ =>`) ;
  `HarnessDescriptor.surface` champ additif projeté depuis la table (engine) ;
  ligne de capacités par agent dans Settings → Agents, muette sur une surface
  inconnue ; annonces ACP `elicitation.form` / `session.compaction` derrière
  `KOMET_ACP_ELICITATION` / `KOMET_ACP_COMPACTION` en **liste de slugs** (un
  `on` global est refusé), donc identiques à aujourd'hui par défaut.
  Tests : `crates/harness/tests/surfaces.rs` ×4, ACP dorés ×3, engine
  `catalog_surfaces_are_the_harness_table_projected_not_recopied`, UI ×2.
- `komet-proto` : variant `AgentEvent::ToolProgress { id, text, append }`
  (additif, jamais plié dans le doc, jamais journalisé).
- `komet-doc` : `parts::latest_plan` (dernier `ToolCall::Todo`, balayage
  arrière) + 4 tests verts ; le fold ignore explicitement `ToolProgress`.
- `komet-harness` : `normalize::plan_events` mappe `turn/plan/updated` sur
  `LIVE_PLAN_TOOL_ID` (snapshot complet, plan vide = effacement) ; les plans
  d'enfants restent consommés ; `progress_events` mappe les deltas de sortie
  Codex (`append: true`), le contenu intermédiaire ACP et le `visibleOutput`
  Muse (`append: false`) ; 5 tests verts (196/196 lib).
- `komet-engine` / `komet-rpc` : coalescence par outil sur 100 ms, queue bornée
  à 4 Ko, broadcast seul (`Inner::broadcast_ephemeral`) ; méthode stream
  `WatchActivity` (forwardable, relayée) qui ferme le flux à `Done` ; 3 tests
  verts (218/218 lib).
- `komet-ui` : `AppState::active_plan` (cache, purge au changement de chat et
  à la troncature) ; carte Plan persistante au-dessus du compositeur ; badge
  `agent running/done/failed` sur les chips de spawn, fingerprinté ; watch
  `tool_progress` résolu au paint, section « Live output » sous la chip ouverte
  seule ; 7 tests UI verts (`active_plan` ×3 dont le frame en place,
  `subagent_status` ×1, `live_output_detail` ×1, carte Plan ×2).
- Reste à livrer : Task 0 (surface déclarée), réponses durables, onglet et
  transcript de sous-agent, compaction, connexion/backoff, clavier ; côté
  Task 7, il ne reste que des non-tests : le rendu de la carte Plan à l'œil
  (`✓`/`○`, compteur, absence de focus volé) et la coalescence mesurée sur un
  Codex réel plutôt que sur le buffer.

## Non-goals

- Modifier le sandbox ou les politiques d'accès natives (chantier séparé).
- Répliquer les sorties d'outils complètes dans le doc (décision A2 de
  `docs/chat2-sync.md` : journal-only, fetch à la demande).
- Synchroniser les transcripts de sous-agents, ni le texte des résumés de
  compaction par défaut (opt-in `SummaryInDoc` uniquement).
- Changer le transport des réponses : `RespondInput` et `Permit` restent la
  seule voie durable, offline-tolerant et multi-device.
- Réécrire le protocole d'un CLI : quand un signal n'existe pas, on le déclare
  absent au lieu de l'inventer.

## Objectif UX

Ces principes arbitrent les détails d'implémentation ; une PR qui les contredit
est refusée même si elle « fonctionne ».

1. **Zéro attente bloquante.** Une question, une approbation, une reconnexion
   ou un run en cours n'empêchent ni la frappe, ni la lecture, ni un steer. Ce
   qui ne peut pas partir tout de suite est mis en file et le dit.
2. **L'état est visible sans clic.** L'utilisateur distingue moteur prêt,
   harness en démarrage, modèle en attente, run en cours, reconnexion et échec.
   Un spinner sans nom est un bug.
3. **Optimiste, jamais menteur.** L'écho local s'affiche immédiatement ; il est
   remplacé par l'accusé du doc, jamais dupliqué. Un état optimiste expiré
   redevient la vérité, il ne reste pas « envoyé » pour toujours.
4. **Le clavier suffit.** Envoyer, steer, stop, approuver, refuser, répondre,
   relancer et ouvrir une demande ont un raccourci et un ordre de focus
   prévisible. La souris n'est jamais le seul chemin.
5. **Continuité visuelle.** Une reconnexion, un changement d'environnement ou
   l'arrivée d'une ligne ne vident pas le transcript et ne déplacent pas le
   scroll si l'utilisateur est remonté. On garde la dernière frame utile.
6. **La latence est dite, pas masquée.** Chaque attente a un délai, une raison
   et une sortie (retry, stop, logs). Pas d'animation qui simule un progrès
   inexistant.
7. **Une animation, un coût mesuré.** Les orbes et transitions réutilisent
   `thinking_orbs.rs`/`motion.rs` et le pulse clock existant ; aucun nouveau
   timer par frame, aucun layout thrash.

## Global Constraints

- **Additif seulement.** Tout nouveau champ serde a `#[serde(default)]` (ou
  `skip_serializing_if`). Un vieux doc, un vieux wire et un vieux client doivent
  continuer à parser et à se re-sérialiser sans les nouvelles clés.
- **Deny gagne toujours.** La règle de merge des permissions
  (`doc/src/parts.rs`, bras `PermissionResolved`) ne change pas. Une réponse
  concurrente à une question est marquée en conflit, jamais écrasée en silence.
- **Thin doc.** Le doc parent ne reçoit jamais : le texte d'un sous-agent, une
  sortie d'outil complète, ni un flux de progression. Les caps existants
  (160 caractères pour un échec d'outil) restent. Le compteur d'outils d'un
  sous-agent n'est écrit qu'à l'état terminal, jamais à chaque outil enfant.
- **Journal = ressource du chat.** Le journal de run devient une surface
  utilisateur (transcript de sous-agent, résumé de compaction) : il est
  supprimé avec le chat, couvert par la rétention, et jamais synchronisé.
  `RunJournal::discard` doit être branché sur `DeleteChat` — aujourd'hui
  `purge_chat` ne supprime que le snapshot (`doc_host.rs:1857`) et le journal
  survit à la suppression.
- **Éphémère ≠ journalisé.** `ToolProgress` et les trames `WatchActivity` ne
  passent jamais par `journal.append`. Le pipeline de publication gagne un
  chemin éphémère (broadcast seul) ; sinon chaque chunk gonfle le journal et
  le replay.
- **Un seul writer par fait durable.** Une réponse `InputResponse` et une
  décision `Permission` sont écrites par un seul chemin, avec une clé
  d'idempotence (`request_id`, appareil, op) ; le fold ne réécrit jamais ce que
  le writer a déjà posé.
- **Redaction.** Le journal est le seul endroit qui peut porter un secret
  local (réponse libre, résumé) : il n'est ni loggé, ni synchronisé, ni inclus
  dans l'export. Les événements diffusés en live sont expurgés du texte du
  résumé et des réponses `secret`.
- **Continuité A2.** Le texte du résumé de compaction suit la même politique
  que les sorties d'outils : journal-only par défaut, fetch à la demande. Le
  doc ne porte que les métadonnées (`tokens`, `trigger`, `summary_bytes`,
  `summary_in_journal`) sauf opt-in explicite `compaction_summary_in_doc`.
- **Aucun no-op silencieux.** Si un harness ne peut pas honorer une action,
  la matrice le dit, l'UI masque l'affordance ou affiche un refus explicite.
  C'est la règle déjà imposée pour le sandbox (`ARCHITECTURE.md` §6).
- **Ordre = ordre du doc.** Les parties d'une entrée sont ordonnées ; les faits
  inter-tour vivent dans des entrées `MessageRole::System` (le rôle existe déjà,
  `doc/src/schema.rs:35`). Ne pas trier le transcript par `created_at`.
- **Sécurité.** Une réponse libre peut contenir un secret : pas de log, pas de
  replay dans un prompt, champ `secret` masqué dans l'UI et dans l'export.
- **Perf.** Les nouvelles lignes réutilisent le cache de vues
  (`Entity::cached`, `RowKind` fingerprinté). `crates/ui/src/perf.rs` mesure
  aujourd'hui la fenêtre et trois enfants (`Transcript`, `Composer`, `Changes`)
  mais **pas** `ContextUsage` ni les lignes individuelles ; Task 8 Step 0 ajoute
  ces compteurs avant toute mesure. Aucune nouvelle boucle par frame.
- **Vérification avant PR** (AGENTS.md) :
  `cargo fmt --all`, `cargo check --workspace --all-targets`,
  `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo test --workspace`. Pour une itération ciblée :
  `cargo test -p komet-harness --test acp`, `cargo test -p komet-doc`,
  `cargo test -p komet-ui`.

---

## 0. Invariants de transparence et d'interaction

Chaque invariant devient un critère d'acceptation et, quand c'est possible, un
test.

- **I1 — Miroir.** Toute action entreprise par le CLI (outil, plan, sous-agent,
  erreur, question, permission, compaction) apparaît dans le transcript au
  moment où elle se produit, avec le même niveau de détail que le CLI.
- **I2 — Persistance.** Question, réponse, décision, frontière et métadonnées
  de compaction survivent au restart, au replay et à l'ouverture sur un autre
  appareil. Pour un sous-agent, seuls le statut, la durée, le nombre d'outils
  et un résumé borné traversent les appareils ; **le transcript reste
  host-local** (journal de run) et son absence est annoncée, pas simulée.
- **I3 — Non-blocage.** Une question ou une approbation n'empêche ni la frappe,
  ni la lecture, ni un steer. Les demandes forment une file ordonnée.
- **I4 — Honnêteté.** La matrice de capacités est la seule source de vérité des
  affordances. Un harness sans canal de réponse ne montre pas de champ de
  réponse.
- **I5 — Fluidité.** Les mises à jour (compaction, progression, sous-agent) sont
  des parties ou des événements éphémères, jamais un re-rendu complet du
  transcript ; le scroll ne saute pas. Vérifiable avec des compteurs **par vue
  et par ligne** (Task 8 Step 0), pas avec le seul total de fenêtre.
- **I6 — Sécurité.** Deny gagne toujours. **Le niveau d'accès est la
  délégation** : en *Full access* l'agent agit sans demander et Komet n'écrit
  aucune ligne de permission par appel — le niveau (chip du composer + requête
  de run) est la trace, c'est le but même de ce niveau. En *Read only*
  l'auto-refus suit la même règle et l'outil refusé reste visible. Hors
  délégation, aucune décision de Komet n'est silencieuse ; un timeout est
  visible.
- **I7 — Connexion lisible.** À tout instant, l'utilisateur sait si le moteur
  est prêt, si le harness démarre, si le modèle réfléchit, si le run tourne, si
  la connexion se rétablit ou si elle a échoué. Un lien perdu conserve la
  dernière frame utile et propose une reprise ; il ne vide jamais l'écran.
- **I8 — Interaction directe.** Envoyer, steer, arrêter, approuver, refuser,
  répondre et relancer ne demandent jamais d'attendre la fin d'un tour ni de
  changer de surface. Chaque action fréquente a un raccourci clavier et un
  ordre de focus prévisible ; aucun raccourci ne peut approuver par accident.
- **I9 — Exhaustivité.** Aucun fait observable du CLI n'est laissé sans
  destination explicite : doc (durable), journal de run (host-local),
  `WatchActivity` (éphémère) ou `None` déclaré dans la matrice. Le tableau
  ci-dessous est la checklist de revue ; un fait absent du tableau est un trou
  de transparence, un fait affiché sans destination est une invention.

### 0.1 Rien n'est ignoré — inventaire des faits observables

Chaque ligne doit être vraie à la fin du chantier. « Visible » nomme la
surface où l'utilisateur le voit **sans action** ; « Récupération » nomme le
chemin pour revoir le détail après coup.

| Fait observable | Destination | Visible dans | Récupération |
| --- | --- | --- | --- |
| Prompt utilisateur | doc (`MessageRole::User`) | écho optimiste puis fil | doc |
| Texte assistant | doc (`MessagePart::Text`) | fil, en flux | doc |
| Réflexion du modèle | non conservée (décision existante) | trailer « modèle en attente » puis texte | — |
| Appel d'outil | doc (`MessagePart::Tool`, métadonnées) | chip dans le fil | doc |
| Sortie d'outil | journal hôte (+ sidecar si présent) | chip ouverte, à la demande | `FetchToolBlob` / `GetToolDetail` |
| Échec d'outil | doc (`is_error` + résumé 160 c) | chip en échec + détail | doc |
| **Plan / tâches** | doc (`ToolCall::Todo`, `LIVE_PLAN_TOOL_ID`) | **carte Plan persistante** au-dessus du composer + chip | doc |
| Progression d'outil | éphémère (`WatchActivity` `toolProgress`) | chip ouverte | live seulement, déclaré `None` après coup |
| Sous-agent : spawn, statut, compteur | doc (chip parente, écriture terminale) | chip (badge `agent running/done/failed`) + onglet Subagent | doc |
| Sous-agent : transcript imbriqué | journal hôte | onglet Subagent, en flux | `WatchSubagent` (hôte seulement) |
| Permission | doc (`MessagePart::Permission`) | carte + file de demandes | doc |
| Question | doc (`MessagePart::Input`) | carte historique + dock actif | doc |
| Réponse à une question | doc (`Input.responses`) | carte résolue, gagnant/conflit | doc |
| Compaction | doc (`MessagePart::Compaction`) | ligne dans le fil + jauge | doc + `GetCompactionSummary` |
| Erreur de tour | doc (`MessagePart::Error`) | ligne + Relancer si `retryable` | doc |
| Usage / fenêtre de contexte | doc (`ContextWindow`) | jauge de contexte | doc |
| Steer | doc (`Steered` / entrée user) | fil | doc |
| Cycle de session | doc (statut) | indicateur Working/Input/Failed/Done | doc |
| Phase de run (démarrage, attente modèle) | éphémère (`RunPhase`) | trailer nommé | live seulement |
| État de connexion | local (`ConnectionStatus`) | bandeau + gate | local |

Règles attachées :

- La carte Plan est **obligatoire**, pas optionnelle : un changement de plan
  qui n'apparaît que dans une chip transitoire est considéré comme ignoré.
- Une liste de tâches est un instantané complet : la **dernière** gagne. Une
  liste vide signifie « plan effacé », pas « aucune donnée » — la carte
  disparaît alors, elle ne garde pas une liste fantôme.
- La progression d'outil et la phase de run ne sont jamais journalisées ; leur
  absence après coup est affichée (`Final`, pas de faux flux).
- Un sous-agent sans transcript disponible (viewer distant, journal rollé)
  affiche un statut et « transcript non disponible sur cet appareil » — jamais
  un vide silencieux.

---

## 1. Modèle canonique

### 1.1 `crates/proto/src/agent.rs` — nouveaux événements

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CompactionStatus {
    InProgress,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum AgentEvent {
    // … variantes existantes inchangées …

    /// Frontière de compaction. `id` est fourni par l'agent quand il en a un
    /// (ACP `compactionId`), sinon synthétisé par l'adaptateur **par
    /// compaction** — jamais un id stable par run : une session peut compacter
    /// plusieurs fois et chaque cycle doit produire sa propre partie.
    #[serde(rename_all = "camelCase")]
    ContextCompaction {
        id: String,
        status: CompactionStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        trigger: Option<String>,       // "auto" | "manual" | valeur agent
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tokens_before: Option<u64>,
        /// Fourni par l'agent quand il le sait ; sinon dérivé du prochain
        /// `ContextWindow` natif après `Completed`. `None` = unknown, l'UI
        /// n'affiche pas la flèche (pas de « 165k → ? »).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tokens_after: Option<u64>,
        /// Remplacement complet du résumé accumulé.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
        /// Ajout au résumé accumulé (ACP `compaction_summary_chunk`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary_delta: Option<String>,
        /// Raison d'un `Failed` (abandon au restart, erreur agent).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },

    /// Progression d'un outil encore en cours. Éphémère : jamais dans le doc.
    #[serde(rename_all = "camelCase")]
    ToolProgress {
        id: String,
        chunk: String,
    },
}
```

Extensions de variantes existantes :

```rust
PermissionRequested {
    // … champs actuels …
    /// Tool call concerné, quand le harness le sait (ACP, Codex, Claude).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

InputRequested {
    request_id: String,
    questions: Vec<UserInputQuestion>,
    /// Origine de la question. Le fold en a besoin pour remplir
    /// `MessagePart::Input.source` ; absent sur un vieux wire → `Native`.
    #[serde(default)]
    source: QuestionSource,
}

InputResolved {
    request_id: String,
    /// Réponses effectivement remises à l'agent. Vide sur un vieux wire.
    #[serde(default)]
    answers: Vec<UserInputAnswer>,
}

pub struct UserInputAnswer {
    pub question_id: String,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub free_text: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum QuestionSource {
    #[default]
    Native,
    Elicitation,
    PermissionShaped,
}

pub struct UserInputQuestion {
    pub id: String,
    pub header: String,
    pub question: String,
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default)]
    pub multi_select: bool,
    /// Description par option, alignée sur `options` (ACP `oneOf`/`anyOf`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub option_descriptions: Vec<String>,
    /// Réponse libre acceptée (toujours vrai pour une question texte).
    #[serde(default)]
    pub allow_free_text: bool,
    /// Champ sensible : l'UI masque la saisie et l'export l'omet.
    #[serde(default)]
    pub secret: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}
```

### 1.2 `crates/doc/src/parts.rs` — parties persistées

```rust
MessagePart::Compaction {
    id: String,
    compaction_id: String,
    status: komet_proto::CompactionStatus,
    trigger: Option<String>,
    tokens_before: Option<u64>,
    tokens_after: Option<u64>,
    /// Raison d'un `Failed` ; le type la portait manquante en v2.
    error: Option<String>,
    /// Rempli seulement quand la politique vaut `SummaryInDoc` : borné à
    /// COMPACTION_SUMMARY_MAX_BYTES (troncature sur frontière UTF-8), jamais
    /// une valeur par défaut.
    summary: Option<String>,
    /// Taille du résumé original, connue même en mode journal-only.
    summary_bytes: Option<u64>,
    /// Un résumé complet existe dans le journal de run de l'hôte et peut être
    /// fetché. Un viewer distant ne l'écrit jamais à `true` par sync.
    summary_in_journal: bool,
},

/// `JournalOnly` (défaut) ou `SummaryInDoc`, piloté par le réglage
/// `compaction_summary_in_doc` (défaut `false`).
pub enum CompactionSummaryPolicy { JournalOnly, SummaryInDoc }

MessagePart::Input {
    // … champs actuels …
    /// Append-only : toutes les réponses, y compris les perdantes (conflict).
    /// `responses[0]` est le gagnant d'affichage ; `applied` marque celle qui
    /// a réellement été remise à l'agent.
    responses: Vec<InputResponse>,
    source: komet_proto::QuestionSource,
    // Pas de `decided_by` séparé : l'appareil et l'horodatage vivent dans
    // `InputResponse`, pour éviter deux vérités qui divergent.
},

MessagePart::Permission {
    // … champs actuels (decision, actions, resolved) …
    tool_call_id: Option<String>,
    decided_by: Option<String>,
},
```

`MessagePart::Tool` gagne deux champs bornés (pas de texte) :

```rust
subagent_duration_ms: Option<u64>,
subagent_tool_count: Option<u32>,
```

Ces deux champs sont écrits **une seule fois**, à la transition terminale
(`Done`/`Failed`). À la récupération au restart, le nombre d'outils est
reconstructible depuis le journal ; la **durée ne l'est pas** (le journal ne
stocke que `{seq, event}`, sans horodatage) : elle vaut `None` si le run est
mort avant le terminal. Le décompte live (« 3 outils ») vient de
`WatchActivity`, jamais du doc synchronisé.

Règles de fold (`fold_event_into_parts`) :

- `ContextCompaction` : cherche `id` (un id **par compaction**) ; met à jour
  en place. En mode `SummaryInDoc`, `summary` remplace et `summary_delta`
  ajoute ; en mode `JournalOnly` (défaut), le texte n'entre pas dans le doc,
  `summary_bytes` est posé et `summary_in_journal = true` **seulement si un
  résumé existe**. Crée la partie si absente, y compris pour un `Completed`
  inter-tour ; `error` est posé sur `Failed`.
- Une réponse à une question s'accumule dans `responses` (append-only) ; le
  writer unique marque `applied` sur celle qu'il remet à l'agent. Voir le
  tie-break §1.5.
- `ToolProgress` : **aucun fold** (éphémère et **non journalisé**).
- `InputResolved` : tamponne `answers` sur la partie dont `request_id`
  correspond — **uniquement si le writer direct n'a pas déjà écrit la
  réponse** (idempotence `request_id` + appareil).
- `PermissionResolved` : inchangé, plus `decided_by` fourni par le writer.
- `Subagent` : inchangé (statut), jamais le contenu. La durée et le compteur
  sont écrits par l'engine au terminal, pas par le fold.

### 1.3 Persisté vs éphémère

| Fait | Persisté dans le doc | Canal éphémère |
| --- | --- | --- |
| Compaction (début, fin, échec) | oui, `MessagePart::Compaction` | `WatchActivity` pour l'état `inProgress` |
| Résumé de compaction | métadonnées seulement (`summary_bytes`, `summary_in_journal`) ; texte si opt-in `SummaryInDoc` | journal de run, fetch `GetCompactionSummary` |
| Progression d'outil | non | `WatchActivity` (`toolProgress`) ; **non journalisée** |
| Sous-agent : statut/durée/nb d'outils | terminal uniquement, sur la chip parente | `WatchActivity` (statut + décompte + preview une ligne) ; **non journalisée** |
| Sous-agent : transcript | non — projection du journal de run, host-local | `WatchSubagent` (subscribe puis replay) |
| Question et réponse | oui, partie `Input` | — |
| Permission et décision | oui, partie `Permission` | — |
| Plan | oui, `ToolCall::Todo` sur `LIVE_PLAN_TOOL_ID` | carte persistante dérivée du doc (Task 7) | — |
| Journal de run | non (hors doc) | ressource hôte du chat : supprimé avec lui, couvert par la rétention, jamais synchronisé |

Le texte du résumé et des réponses `secret` n'est pas diffusé en live : l'engine
les retire de l'événement `WatchActivity`/broadcast ; ils restent dans le
journal local et ne sortent que par `GetCompactionSummary` / la partie `Input`
selon la politique.

### 1.4 Ordre et insertion

- Pendant un tour : les parties s'ajoutent à l'entrée assistant en streaming.
- Hors tour (auto-compaction entre deux tours, erreur de session) : l'engine
  insère une entrée `MessageRole::System` avec la partie, `status: Complete`.
- Le doc est la source de vérité de l'ordre. `created_at` sert à l'affichage
  d'horodatage, jamais au tri.
- `rail.rs`, `stats.rs` et `export.rs` ignorent déjà `System` ; vérifier qu'ils
  n'affichent pas une entrée vide.

### 1.5 Conflit de réponses — règle déterministe

`MessagePart::Input` gagne `responses: Vec<InputResponse>` (append-only, jamais
écrasé) :

```rust
pub struct InputResponse {
    pub answers: Vec<komet_proto::UserInputAnswer>,
    /// Horodatage d'affichage de l'appareil qui a répondu. Jamais utilisé pour
    /// décider — les horloges ne sont pas comparables entre appareils.
    pub decided_at: String,
    pub device: String,
    /// Vrai pour la réponse effectivement remise à l'agent par l'hôte.
    /// Le gagnant du merge n'est pas forcément celui que l'agent a reçu.
    #[serde(default)]
    pub applied: bool,
}
```

- **Gagnant = premier élément de `responses` dans l'ordre du doc.** Loro ordonne
  les insertions concurrentes de façon déterministe et identique sur tous les
  pairs ; deux réplicas convergent donc sur le même gagnant. Aucun tri par
  `decided_at`. Le statut « perdante » est **dérivé de la position** (index >
  0), pas stocké : rien à réécrire au merge.
- Une réponse non retenue reste dans la liste et est affichée « réponse
  concurrente ignorée (appareil X) ».
- L'engine ne transmet à l'agent que la réponse locale qu'il a lui-même
  ajoutée ; une réponse distante arrivée après résolution n'est jamais
  rejouée dans le prompt.
- **Gagnant du merge ≠ réponse remise.** L'engine marque `applied` sur la
  réponse qu'il a effectivement transmise. Si l'ordre du doc élit une autre
  réponse (cas hors ligne), l'UI affiche les deux explicitement — « remise à
  l'agent : X » et « concurrente ignorée : Y » — et ne laisse jamais croire
  que l'agent a reçu le gagnant du merge.
- Si `responses` est absent (vieux doc) : `resolved` seul fait foi et
  `responses: []` (pas de rétro-invention).
- Test : deux réplicas qui ajoutent une réponse hors ligne fusionnent et
  élisent le même gagnant ; l'autre est rendue comme conflit (index 1).

---

## 2. Matrice de capacités cible

Valeurs finales visées. `Elicitation` / `Lifecycle` côté ACP signifient « le
client Komet sait le recevoir **si** l'annonce expérimentale est activée » ;
l'agent peut ne rien envoyer, et la matrice le dit. Les champs ACP
(`compaction_update`, `compactionId`, `compaction_summary_chunk`,
`elicitation/create`, `clientCapabilities.elicitation.form` /
`session.compaction`) viennent de **RFD à l'état de brouillon** : ils sont
traités comme des signaux optionnels, jamais comme un contrat. Si l'agent ou
l'adaptateur épinglé ne les émet pas, la surface effective reste `None` et la
matrice doit être corrigée avant merge (Gate B).

| Agent | Questions | Compaction | Sous-agents | Plans | Progrès d'outil |
| --- | --- | --- | --- | --- | --- |
| Claude Code | `Native` (`AskUserQuestion`) | `LifecycleWithSummary` (`status` + `compact_boundary` + `isCompactSummary`) | `Tagged` | `ToolChip` (`TodoWrite`) | `None` (le CLI ne streame pas stdout) |
| Codex | `None` (approvals seulement) | `BoundaryOnly` (`thread/compacted`) | `Tagged` (threads enfants) | `LivePanel` (après mapping) | `Streaming` (`commandExecution/outputDelta`) |
| Cursor | `None` (pas de canal de réponse SDK) | `BoundaryOnly` (`summary-started/completed`) | `Tagged` (`taskUpdate`) | `ToolChip` | `Final` (le shim ne mappe pas de contenu partiel) |
| Grok (ACP) | `Elicitation` + fallback `PermissionShaped` | `Lifecycle` (RFD draft, gated) | `NestedSession` + tail `chat_history.jsonl` | `ToolChip` | `Streaming` (mappé, émission par agent à vérifier) |
| OpenCode (ACP) | `Elicitation` + fallback | `Lifecycle` (RFD draft, gated) | `NestedSession` | `ToolChip` | `Streaming` (mappé, émission par agent à vérifier) |
| Qoder (ACP) | `Elicitation` + fallback | `Lifecycle` (RFD draft, gated) | `TaggedIfEmitted` | `ToolChip` | `Streaming` (mappé, émission par agent à vérifier) |
| Devin (ACP) | `Elicitation` + fallback | `Lifecycle` (RFD draft, gated) | `TaggedIfEmitted` | `ToolChip` | `Streaming` (mappé, émission par agent à vérifier) |
| GitHub Copilot (ACP) | `Elicitation` + fallback | `Lifecycle` (RFD draft, gated) | `TaggedIfEmitted` | `ToolChip` | `Streaming` (mappé, émission par agent à vérifier) |
| Hermes (ACP) | `Elicitation` + fallback | `Lifecycle` (RFD draft, gated) | `TaggedIfEmitted` | `ToolChip` | `Streaming` (mappé, émission par agent à vérifier) |
| Pi (ACP) | `Elicitation` + fallback | `Lifecycle` (RFD draft, gated) | `TaggedIfEmitted` | `ToolChip` | `Streaming` (mappé, émission par agent à vérifier) |
| Cline (ACP) | `Elicitation` + fallback | `Lifecycle` (RFD draft, gated) | `TaggedIfEmitted` | `ToolChip` | `Streaming` (mappé, émission par agent à vérifier) |
| Antigravity | `Native` (`ask_question`, canal à réparer) | `None` (à sonder) | `None` | `ToolChip` | `Final` (aucune progression mappée) |
| Muse | `Native` (`request_user_input`) | `Lifecycle` (item démasqué) | `Tagged` (`subagent_spawn`) | `LivePanel` | `Streaming` (révisions d'item) |
| Mock | `Native` | `LifecycleWithSummary` | `Tagged` récursif | `LivePanel` | `Streaming` |

**14 harnesses au total, dont 8 ACP** (Qoder inclus). Qoder est un agent ACP
natif (`acp/mod.rs:745`) : il suit le chemin partagé §3.4, pas une variante.

La colonne **Progrès d'outil** déclare ce que **l'adaptateur mappe**, pas ce
qu'un CLI particulier envoie : depuis Task 7 Step 3, le fil ACP et le fil Muse
traduisent le contenu intermédiaire en `ToolProgress`, donc un agent ACP qui
n'en émet jamais reste `Final` **en pratique** sans que le contrat le dise. La
vérification Gate B par agent (version épinglée, update observé ou absent) fixe
la valeur réelle ; en attendant, `Streaming (mappé, émission à vérifier)`.
Hors ACP et Muse, rien n'est mappé : Cursor (SDK via son shim, qui ne transmet
pas de contenu partiel) et Antigravity (fil propriétaire) restent `Final`, et
Claude Code `None` — son CLI ne donne pas de stdout partiel par notification et
rien n'est simulé.

Le test `interaction_surfaces_are_declared` doit échouer si un `HarnessId`
n'a pas de déclaration explicite **ou si sa déclaration diffère de la table
golden** (itérer sur les 14, pas 13).

---

## 3. Tâches

### Task 0 — Surface d'interaction et capacités ACP

**But :** une source de vérité unique, et débloquer les signaux standardisés
auprès des agents ACP.

**Files:**
- Modify: `crates/proto/src/agent.rs` (types `HarnessInteractionSurface`,
  `QuestionSurface`, `CompactionSurface`, `SubagentSurface`, `PlanSurface`,
  `ProgressSurface`).
- Modify: `crates/engine/src/registry.rs` (`HarnessDescriptor.surface`).
- Modify: `crates/harness/src/lib.rs` (méthode de trait
  `fn interaction_surface(&self) -> HarnessInteractionSurface` avec défaut
  `none()`).
- Modify: chaque `crates/harness/src/{claude,codex,cursor,antigravity,muse}/mod.rs`
  et `crates/harness/src/mock.rs`.
- Modify: `crates/harness/src/acp/mod.rs` (`initialize`).
- Modify: configuration des harness : flags désactivés par défaut et **portée
  par `HarnessId`** (liste d'agents autorisés, ou config équivalente). Un flag
  global `on` est refusé : une validation Gate B sur Grok ne doit pas annoncer
  la capacité à OpenCode ou Qoder.
- Décision de source de vérité : la table `HarnessId → surface` vit dans
  `crates/harness` ; `HarnessDescriptor.surface` et Settings la projettent.
  Ne pas déclarer la surface deux fois (registry + trait).
- Modify: `crates/ui/src/settings/harnesses.rs` (affichage de la surface
  **effective**, flags compris).
- Test: `crates/harness/tests/acp.rs`, `crates/harness/tests/claude.rs`,
  `crates/engine/src/registry.rs` (tests) et un nouveau
  `crates/harness/tests/surfaces.rs`.

**Interfaces produites :**

```rust
pub struct HarnessInteractionSurface {
    pub questions: QuestionSurface,
    pub compaction: CompactionSurface,
    pub subagents: SubagentSurface,
    pub plans: PlanSurface,
    pub tool_progress: ProgressSurface,
}

pub enum QuestionSurface { None, PermissionShaped, Elicitation, Native }
pub enum CompactionSurface { None, BoundaryOnly, Lifecycle, LifecycleWithSummary }
pub enum SubagentSurface { None, Tagged, TaggedIfEmitted, NestedSession }
pub enum PlanSurface { None, ToolChip, LivePanel }
pub enum ProgressSurface { None, Final, Streaming }
```

- [x] **Step 1:** Écrire `interaction_surfaces_are_declared` : itérer sur les
  **14** `HarnessId` (Qoder inclus) et comparer à une **table golden** par
  agent (matrice §2), pas seulement vérifier la présence du champ. Un match
  exhaustif dans l'implémentation, pas un `_ =>`. *(Implémenté :
  `crates/harness/tests/surfaces.rs`, 4 tests verts — golden par agent,
  `every_harness_id_has_a_golden_row` (compte 14 + pas de doublon),
  `no_harness_is_declared_as_an_all_zero_silent_surface` (une ligne
  tout-`None` est un no-op déguisé, interdit),
  `plan_and_progress_surfaces_match_the_mapped_wires`. Côté implémentation,
  `capabilities::interaction_surface(HarnessId)` est un `match` exhaustif sur
  les 14, sans `_ =>`.)*
- [x] **Step 2:** Écrire `acp_capabilities_are_unchanged_when_experiments_off` :
  sans `KOMET_ACP_ELICITATION=form` ni `KOMET_ACP_COMPACTION=1`, le JSON
  `clientCapabilities` est **identique à aujourd'hui** (golden). Puis
  `acp_capabilities_advertise_when_enabled` : avec les deux variables, le JSON
  contient `elicitation.form` et `session.compaction`, sans régresser `fs` /
  `terminal`. *(Implémenté dans `acp/mod.rs` : `initialize_params_with(elicitation,
  compaction)` sépare la forme du fil de la lecture d'environnement, donc les
  deux tests dorés sans toucher l'env du process ; `draft_signal_allows` est la
  portée par `HarnessId`. **Écart assumé avec l'exemple du step** : `=form` et
  `=1` sont refusés par conception — la valeur est une **liste de slugs**
  (`KOMET_ACP_ELICITATION=grok,opencode`), sinon un global `on` étendrait la
  validation d'un agent à huit. Test dédié
  `draft_signals_are_allowed_per_agent_and_never_globally`.)*
- [ ] **Step 2b:** Valider l'annonce **agent par agent** (Grok, OpenCode,
  Qoder, etc., version épinglée) avant d'ajouter un `HarnessId` à l'allowlist :
  consigner dans `docs/research/acp.md` (version, capacités lues, update reçu
  ou absence). Tant qu'un agent n'est pas validé, il reste hors allowlist et
  la matrice décrit sa surface effective, pas la surface espérée. *(Non fait :
  `docs/research/acp.md` ne contient encore aucune ligne elicitation/compaction,
  donc **aucun** agent n'est en allowlist et l'annonce expérimentale reste
  morte en pratique — c'est l'état attendu, pas un oubli d'implémentation.)*
- [x] **Step 3:** Implémenter les types et les overrides par harness selon la
  table §2. Pour les ACP (Qoder inclus), une seule implémentation partagée,
  paramétrée par `HarnessId` pour les cas `TaggedIfEmitted`. Tant que Gate B
  est fermée pour un agent, sa déclaration compaction/questions reste
  `None`/`PermissionShaped`. *(Implémenté : types dans `komet_proto::agent`
  (`HarnessInteractionSurface` + 5 enums, `serde` kebab/camelCase, `Default` =
  tout-absent = « inconnu ») ; `acp_surface(SubagentSurface)` factorise les 8
  ACP en une seule déclaration ; `NestedSession` pour Grok et OpenCode,
  `TaggedIfEmitted` pour les six autres.)*
- [x] **Step 4:** Ajouter `surface` à `HarnessDescriptor` **dérivé de la table
  harness** (pas une seconde déclaration) et l'afficher dans Settings → Agents
  sous forme de lignes courtes (« Compaction : oui », « Questions : non
  supportées »). *(Implémenté : le champ porte `#[serde(default)]` et chaque
  constructeur — `describe()` pour un harness résolu, chaque slot lazy de
  `default_registry` — appelle `capabilities::interaction_surface(id)`, donc le
  descripteur ne recopie jamais une valeur ;
  `catalog_surfaces_are_the_harness_table_projected_not_recopied` (engine) le
  vérifie sur les 14 lignes du catalogue. Settings → Agents affiche une ligne
  de capacités par agent (`surface_phrases`), **une seule ligne jointe par `·`
  plutôt que cinq** pour garder la rangée lisible ; une surface inconnue
  (catalogage d'un moteur plus ancien) n'affiche **rien** au lieu de prétendre
  « non supporté ». Deux tests UI verts.)*
- [x] **Step 5:** Exécuter `cargo test -p komet-harness --test surfaces`,
  `cargo test -p komet-engine registry`, `cargo check -p komet-ui`. *(2026-10-29
  : surfaces 4/4, engine `registry::` 19/19 (219/219 lib), harness lib
  199/199, UI 612/612, `cargo clippy -p komet-harness -p komet-engine -p
  komet-proto --all-targets` sans nouvelle alerte.)*

**Critère :** aucune affordance UI ne peut être affichée sans une capacité
déclarée ; par défaut aucun agent ACP ne voit d'annonce expérimentale ; une
fois Gate B validée **pour un agent**, cet agent seul reçoit les deux
capacités. Les 14 harnesses ont une ligne de matrice et une ligne Settings.

---

### Task 1 — Réponses et décisions visibles dans le fil

**But :** arrêter de perdre les réponses et les décisions.

**Files:**
- Modify: `crates/proto/src/agent.rs` (`InputResolved.answers`,
  `PermissionRequested.tool_call_id`).
- Modify: `crates/doc/src/parts.rs` (`Input.responses`, `source` ;
  `Permission.tool_call_id`, `decided_by` ; fold).
- Modify: `crates/doc/src/schema.rs` (`DocPartJson` champs additifs ;
  `resolve_input` ajoute une `InputResponse`, ne remplace jamais la liste).
- Modify: `crates/engine/src/sessions.rs` (`respond_input` émet les réponses ;
  `respond_permission` transmet `tool_call_id` et le device).
- Modify: `crates/engine/src/doc_host.rs` (writer de `decided_by` et de
  l'`InputResponse`, y compris pour un run déjà mort).
- Modify: `crates/ui/src/transcript.rs` (`RowKind::Permission`, `RowKind::Question`).
- Modify: `crates/ui/src/composer.rs` (`pending_input_requests`, file de demandes).
- Test: `crates/doc/src/schema.rs` (round-trip vieux doc),
  `crates/engine/tests/restart_resume.rs`, `crates/ui` (rows).

**Steps:**
- [x] **Step 1:** Test `old_input_part_without_responses_still_reads` : un doc
  contenant `{"kind":"input","id":"in-r1","requestId":"r1","questions":[],"resolved":true}`
  se relit avec `responses: []`, `source: Native`.
- [x] **Step 2:** Test `input_resolved_appends_a_response` : `resolve_input("r1",
  answers)` ajoute les libellés et le texte libre à `responses`, et un replay
  les restitue. Test jumeau `input_requested_carries_source` : une question
  `PermissionShaped` se relit avec la bonne `source`, pas `Native`.
- [x] **Step 3:** Test `concurrent_answers_converge_on_doc_order` : deux
  réplicas ajoutent une réponse hors ligne, fusionnent, et élisent le même
  gagnant (premier élément dans l'ordre du doc, §1.5) ; le perdant est affiché
  comme conflit (dérivé de l'index) et n'est jamais transmis à l'agent.
  Test `delivered_answer_is_marked` : `applied` reste sur la réponse remise,
  même si le merge élit une autre réponse en position 0.
- [x] **Step 4:** Implémenter le fold et **un seul** writer ; brancher
  `respond_input` pour transmettre les réponses à l'événement. Le chemin
  dead-run et le chemin live passent par le même helper idempotent
  (`request_id`, appareil, op) : aucun doublon dans `responses`.
- [x] **Step 5:** Remplacer `pending_input_request` et
  `pending_permission_request` par des fonctions retournant **toutes** les
  demandes ouvertes, avec `entry_id`, ordre d'arrivée et un drapeau
  `actionable` (une question d'un run mort est affichée comme historique, pas
  comme formulaire actif).
- [x] **Step 6:** Rendre `PermissionRow` et `QuestionRow` : icône, résumé,
  décision/réponse, device, horodatage, lien vers le tool call. Remplacer le
  chip passif `InputChip` (`transcript.rs`, fonction `input_chip`).
- [x] **Step 7:** `cargo test -p komet-doc && cargo test -p komet-ui`.

**Critère :** l'historique affiche « Question : … → réponse : … », la source
de la question, la réponse effectivement remise à l'agent (`applied`) et
« Permission : git push → Allow (device X, 14:32) », y compris après restart.

**Statut (implémenté) et écarts assumés :**

- Écrivain unique : `SessionDoc::record_input_response` (doc) appelé depuis
  `doc_host::execute` (le exécuteur de commande), avec `entry.issued_by` et
  `entry.issued_at` comme appareil/horodatage. Le **fold n'écrit jamais**
  `responses` : il ne connaît ni l'appareil ni l'horloge et ajouterait un
  doublon fantôme pendant le streaming ; `InputResolved` ne fait que poser
  `resolved`. Les chemins live et dead-run passent par le même helper,
  idempotent par appareil.
- `decided_at` est un `i64` (millis epoch), aligné sur `SessionCommandEntry`.
- `responses` est un **conteneur LoroList matérialisé dès `push_part`** (même
  vide) : deux appareils qui créeraient le conteneur en concurrence sous la
  même clé perdent l'un des deux (conflit de conteneurs), ce qui ferait
  disparaître une réponse. Avec le conteneur partagé, les appendes fusionnent
  et l'ordre du doc élit le même gagnant sur tous les pairs — test
  `concurrent_answers_converge_on_doc_order`.
- Bug préexistant corrigé au passage : `push_part` / `update_part_fields`
  n'écrivaient jamais `errorKind`, `errorDetail`, `errorRetryable`,
  `subagentRef`, `subagentStatus`, `subagentTail`, et `to_doc_part` jetait
  `decision`. Ces faits ne survivaient donc pas au streaming (la Task 5 en
  dépend — elle est réellement durable maintenant).
- Proto enrichi au-delà du plan (additif, `skip_serializing_if`) :
  `UserInputQuestion.{option_descriptions, allow_free_text, secret, default}`
  et `UserInputAnswer.free_text`. Produits par les adaptateurs ACP / Claude /
  Codex / Muse / mock ; le texte saisi dans l'assistant voyage dans
  `free_text` comme dans `labels`.
- `actionable` (composer) : une demande d'une entrée `Streaming` est un
  formulaire ; une demande laissée par un run **abandonné** reste actionnable
  tant qu'aucune entrée assistant plus récente ne l'a remplacée — c'est le
  chemin « la réponse reprend le run » du dead-run, exigé par l'utilisateur ;
  une demande non résolue d'une entrée `Complete` est de l'historique.
  L'énoncé du plan (« run mort ⇒ historique ») est donc précisé, pas inversé.
- Une seconde réponse sur une question déjà répondue est **enregistrée comme
  conflit** (`applied: false`) et la commande est `Rejected` — elle n'est
  jamais redépatchée. Test moteur
  `answered_question_is_durable_and_a_second_answer_is_a_conflict`.
- Remis : le « lien » vers le tool call est pour l'instant l'**id affiché**
  sur la ligne Permission (pas de saut/scroll — la surface des actions inline
  est la Task 11) ; les `option_descriptions` d'AskUserQuestion ne sont pas
  encore rendus par l'assistant ; les questions ACP façonnées par permission
  reconvervent leur `request_id` sous l'id de l'option, l'id d'option n'étant
  pas conservé sur `UserInputQuestion.options`.
- Vérifications : `cargo test -p komet-doc` (99 dont 6 nouveaux),
  `cargo test -p komet-ui` (613), `cargo test -p komet-engine` — tout passe
  sauf `uploads_chunk_commit_readback_and_jail`, **échec préexistant hors
  périmètre** (`uploads.rs` et son test intouchés : la jail accepte
  volontairement un non-image dans le cache d'upload, le test l'exige refusé).
  Le test `empty_reasoning_deltas_are_heartbeats_not_journal_noise` attendait
  une entrée `Complete` quelconque (l'écho utilisateur) et passait donc par un
  hasard de ordonnancement ; il attend désormais l'entrée assistant.
- `cargo fmt --all --check` propre ; `cargo clippy --workspace --all-targets`
  propre hors le `dead_code` de `SpacesMenuPage::Projects`
  (`crates/ui/src/shell/spaces.rs:36`), qui vient des modifications non
  committées de l'utilisateur sur `feat/environments` (ses trois sites de
  construction ont été supprimés) et non de cette tâche — donc laissé tel quel.
  Deux corrections rendues nécessaires par le passage clippy : l'attribut
  `#[allow(clippy::too_many_arguments)]` de `drive_run`, déplacé malgré lui sur
  `flush_tool_progress` par une insertion, est revenu sur sa fonction ; et
  `question_row` (8 args) prend maintenant un `QuestionStatus`
  (`Answered` / `RunEnded` / `Awaiting`) au lieu du couple de booléens
  `resolved` + `history_only`, ce qui rend l'état incohérent impossible et
  entrée dans le fingerprint de ligne (le passage « run fini » change la
  version de la ligne).
- À ne pas oublier quand `export.rs` rendra les réponses (`Input`) :
  aujourd'hui l'export markdown ignore totalement les parties question et
  permission (`_ => {}`), donc aucun secret ne fuit ; dès qu'il les rendra, le
  `free_text` des questions `secret` doit rester masqué, sous peine de violer
  la contrainte « jamais exporté ».

---

### Task 2 — Compaction : socle proto/doc/engine/UI

**Files:**
- Modify: `crates/proto/src/agent.rs` (`CompactionStatus`,
  `AgentEvent::ContextCompaction`).
- Modify: `crates/doc/src/constants.rs` (`COMPACTION_SUMMARY_MAX_BYTES = 8 * 1024`,
  `CompactionSummaryPolicy`).
- Modify: `crates/doc/src/parts.rs` (`MessagePart::Compaction`, fold).
- Modify: `crates/doc/src/schema.rs` (`DocPartJson` additif).
- Modify: `crates/doc/src/export.rs` (ligne de compaction, réponses `Input`,
  omission des réponses `secret` et du texte journal-only).
- Modify: `crates/engine/src/doc_host.rs` (insertion en entrée `System` hors
  tour ; abandon au restart ; politique de résumé).
- Modify: `crates/engine/src/sessions.rs` (pipeline : route l'événement,
  met à jour les stats existantes, publie `WatchActivity` pour `inProgress`).
- Modify: `crates/engine/src/run_journal.rs` (`compaction_summary(chat,
  compaction_id)` : relit l'événement journalisé).
- Modify: `crates/rpc/src/lib.rs` (`WATCH_ACTIVITY`, `GET_COMPACTION_SUMMARY`).
- Modify: `crates/ui/src/state.rs` (`activity: HashMap<String, RunActivity>`).
- Modify: `crates/ui/src/transcript.rs` (`RowKind::Compaction`).
- Modify: `crates/ui/src/context_usage.rs` (lien « voir dans le fil »,
  cohérence compteur/timeline).
- Test: `crates/doc` (fold), `crates/engine/tests/` (restart),
  `crates/ui` (row), `crates/harness` (fixtures par agent, Task 3).

**Steps:**
- [x] **Step 1:** Test `compaction_part_round_trips_and_old_doc_ignores_it`.
  *(Implémenté dans `crates/doc/src/schema.rs` — l'écriture relit la partie,
  et une ligne mal formée reste rendable au lieu d'être supprimée.)*
- [x] **Step 2:** Test `compaction_fold_updates_in_place_by_id` : `inProgress`
  puis `completed` avec `tokens_before`, `trigger` et `summary` ; un
  `summary_delta` successif s'ajoute ; un `summary` concret remplace.
  Test `two_compactions_in_one_run_get_two_parts` : un second cycle avec un
  nouvel id ne doit pas écraser le premier.
  *(Implémenté : `crates/doc/src/parts.rs` — un `MessagePart::Compaction` par
  id de cycle, rafraîchi en place ; un `InProgress` qui arrive APRÈS un cycle
  réglé ne le rouvre pas. Les trois tests sont verts.)*
- [x] **Step 3:** Test `inter_turn_compaction_creates_a_system_entry` : hors
  stream, une entrée `System` complète est insérée à la bonne position.
  *(Implémenté : `SessionDoc::record_compaction_part`. **Écart** : `SessionDoc`
  n'a pas d'insertion positionnelle — seulement `push_message` en queue — donc
  la boundary hors-tour est UNE entrée `System` appendue en queue, d'id
  `sys-{compaction_id}` et de statut `Complete` ; le `status` de la partie porte
  la vivacité. C'est la bonne position en pratique : une compaction inter-tour
  arrive toujours après le dernier tour clos. Le même id se raffine en place,
  et un second cycle ouvre une seconde entrée.)*
- [x] **Step 4:** Tests de politique :
  `journal_only_summary_never_reaches_the_doc` (défaut : `summary: None`,
  `summary_bytes` posé, `summary_in_journal = true` **seulement si un résumé
  existe**) ; `summary_in_doc_is_capped` (opt-in : 100 Ko tronqués à 8 Ko sur
  une frontière UTF-8, `summary_bytes` conserve la taille d'origine).
  *(Implémenté : `CompactionSummaryPolicy` + `COMPACTION_SUMMARY_MAX_BYTES` dans
  `crates/doc/src/constants.rs`, appliqués par `fold_event_into_parts_with`.)*
- [x] **Step 4b:** Test `get_compaction_summary_reads_the_journal` : fetch sur
  l'hôte → texte ; sur un device sans journal → résultat typé `unavailable`
  (pas le vec vide de `GetToolDetail`), jamais une chaîne vide silencieuse.
  Test `summary_deltas_are_concatenated_in_order` : trois chunks
  `summary_delta` puis un replay doivent produire le résumé complet.
  *(Implémenté : `RunJournal::compaction_summary` (scan du journal, chunks
  concaténés dans l'ordre, un `summary` complet remplace les chunks qu'il
  suit), méthode `SessionsEngine::compaction_summary`, méthode RPC
  `GetCompactionSummary` (params `{chatId, compactionId}`, réponse
  `{status:"ok",text}` ou `{status:"unavailable",reason}`) et `forwardable`
  pour viser l'hôte. **Écart de nommage** : le test s'appelle
  `compaction_summary_reply_distinguishes_absent_from_empty` côté
  `crates/engine/src/rpc.rs`.)*
- [x] **Step 4c:** Test `export_honours_summary_and_secret_policies` : en mode
  journal-only l'export contient la ligne mais pas le texte ; une réponse
  `secret` n'est jamais exportée. Modifier `crates/doc/src/export.rs` en
  conséquence (absent du périmètre v2).
- [x] **Step 5:** Implémenter l'insertion (`fold into streaming assistant` sinon
  `insert_system_entry`), la politique de résumé, la publication
  `WatchActivity`, le fetch journal et l'abandon (`mark_abandoned_streams`
  ferme un `inProgress` en `Failed` avec `error`). Dériver `tokens_after` du
  prochain `ContextWindow` natif quand l'agent ne le fournit pas. Retirer
  `summary`/`summary_delta` de l'événement **diffusé** (le journal les garde).
  *(Implémenté : routeur `route_compaction` dans `crates/engine/src/sessions.rs`
  — trois maisons pour un seul fait : raffiner dans le segment en cours, ouvrir
  une entrée `System` si aucun segment, raffiner l'entrée `System` existante si
  un steer a rouvert la session (jamais un jumeau). `publish_compaction` journalise
  l'événement COMPLET et diffuse une copie sans `summary`/`summary_delta`.
  `SessionDoc::apply_compaction_tokens_after` : premier writer gagne, et un
  cycle plus ancien ne peut pas emprunter le nombre d'un plus récent.
  Abandon : `fail_interrupted_compactions` branché dans `mark_abandoned_streams`.
  **Écart** : pas de nouveau canal `WatchActivity`/`RunActivity` pour la
  compaction — la vivacité est portée par le `status` de la partie et
  l'entrée `Streaming` ; `WatchActivity` reste le canal tool-progress /
  subagents. **Écart** : la politique est pilotée par
  `KOMET_COMPACTION_SUMMARY_IN_DOC` (+ `set_compaction_summary_in_doc`), pas
  par un réglage synchronisé dans la doc — aucun réglage persisté n'était dans
  le périmètre, et `JournalOnly` reste le défaut.)*
- [x] **Step 5b:** Test `interrupted_compaction_is_failed_on_restart` : une
  partie `InProgress` sans run vivant devient `Failed` avec `error` au restart,
  une seule fois (idempotent), sans toucher aux autres parties.
  *(Implémenté sous le nom `interrupted_compaction_is_failed_and_stays_failed`
  — filtre par `deviceId`, ne touche ni `Completed` ni les autres devices.)*
- [x] **Step 6:** UI : ligne compacte « Context compacted · 165k → 12k · auto ·
  14:32 » avec état `in_progress` (pulse discret), résumé dépliable via
  `GetCompactionSummary` (état « disponible sur l'hôte » si le journal est
  absent), et un `RowKind::Compaction` qui ne participe pas au regroupement
  d'outils. Si `tokens_after` est absent, afficher « 165k → ? » sans inventer
  de valeur ; sur `Failed`, afficher `error`.
  *(Implémenté : `RowKind::Compaction` + `compaction_line` +
  `render_compaction_row` dans `crates/ui/src/transcript.rs`, `spawn_recap_fetch`
  calqué sur `spawn_journal_fetch` (dont `targetDeviceId` vers l'appareil qui
  possède le chat), `Unavailable` terminal → « the recap … is in the host's run
  journal ». Le fingerprint de ligne et `entry_fingerprint` couvrent status /
  trigger / tokens / error / summary_bytes / journal — sinon une ligne raffinée
  resterait figée dans le cache. Quatre tests UI verts : ligne sans invention, non-regroupement
  avec les tools, raffinement en place (même id, version différente), `Failed`
  qui porte son `error`.)*
- [x] **Step 7:** Vérifier que le popover « Context » et la timeline racontent la
  même chose (compteur et dernière raison), sans double rendu.
  *(Fait : `AppState::thread_compactions` lit les parties de la doc ; le popover
  « Context » affiche `max(compteur natif, doc)` — seule `codex`/`cursor`
  émettent un compteur natif, les autres laisseraient un « 0 » au-dessus de
  lignes existantes — et garde la raison signalée par l'agent ; il ne montre
  `Latest` que si une raison est
  réellement signalée (le repli inventé « cache expiry » est supprimé). Lien
  « See in thread » → action `RevealLastCompaction` (gérée par le shell : ferme
  le popover, `Transcript::reveal_last_compaction`). Pas de seconde jauge : la
  « squash » demandée existe déjà — l'anneau est `used / context_limit`
  (`ContextUsageStats::used()` = input+output du dernier tour) et le popover
  pose déjà le tick `Compacts at`. Le fil ajoute donc l'ÉVÈNEMENT (quand, pourquoi,
  ce qu'il a sauvé), pas une mesure dupliquée.)*
- [x] **Step 8:** `cargo test -p komet-doc && cargo test -p komet-engine &&
  cargo test -p komet-ui`. *(2026-10-29 : doc 109/109 + 1/1, engine lib
  223/223 et tous les tests d'intégration verts, UI 618/618. Un échec
  **préexistant et hors périmètre** a été corrigé au passage :
  `uploads_chunk_commit_readback_and_jail` attendait encore le refus d'un
  fichier non-image **dans** le cache d'upload, politique élargie volontairement
  par `e83ff8b` (`is_upload` admet tout type connu dans le cache, et
  `application/octet-stream` sinon) sans mise à jour de l'assertion. Le test
  vérifie maintenant les deux garanties réellement voulues : un non-image lu
  via une racine cwd est refusé, un type inconnu hors cache est refusé, et le
  cache continue d'admettre ses propres fichiers. `uploads.rs` n'est pas touché.)*

**Critère :** chaque compaction est visible au bon endroit dans le fil, dans
l'ordre d'arrivée, avec sa propre ligne même si la session compacte plusieurs
fois, et survit au restart.

---

### Task 3 — Compaction : un adaptateur par famille de signal

Les sous-tâches 3.1, 3.2, 3.3 et 3.6 d'une part, 3.4, 3.5 et 3.7 d'autre part,
sont groupées en deux PRs (§6), car elles partagent les mêmes fichiers de fold
et de détection. Le contrat commun : produire `AgentEvent::ContextCompaction`
avec le meilleur signal disponible ;
si aucun signal, ne rien produire et laisser la matrice déclarer
`CompactionSurface::None`. Les huit agents ACP partagent un seul adaptateur
(3.4) ; la couverture **par agent** est vérifiée en Task 5 et §7, pas supposée
ici.

#### 3.1 Claude Code — `crates/harness/src/claude/`

**Signal :** `{"type":"system","subtype":"status","status":"compacting"}` au
début (émis pour `/compact` ; l'auto-compaction peut rester silencieuse jusqu'à
la frontière), puis
`{"type":"system","subtype":"compact_boundary","compact_metadata":{"trigger":"manual","pre_tokens":18875}}`
(accepter aussi `compactMetadata` / `preTokens`), et un message utilisateur
`isCompactSummary` portant le résumé.

**Changement :**
- `wire.rs` : `SystemFrame` gagne `compact_metadata: Option<CompactMetadata>`
  (alias camelCase/snake_case), `status: Option<String>` ; `MessageFrame` gagne
  `is_compact_summary: bool` (alias `isCompactSummary`).
- `normalize.rs` :
  - `subtype == "status"` et `status == "compacting"` → `ContextCompaction {
    status: InProgress, trigger: None }` avec un id synthétique **par
    compaction** (`run-{n}-compaction-{k}`) : un nouvel id à chaque cycle,
    jamais un id stable par run.
  - `subtype == "compact_boundary"` → `ContextCompaction { status: Completed,
    trigger, tokens_before }` sur l'id ouvert par le `status` précédent ; à
    défaut, en ouvrir un nouveau. `tokens_after` reste `None` jusqu'au
    prochain `ContextWindow` natif.
  - message `is_compact_summary` → `ContextCompaction { summary }` sur le même
    id (ou `summary_delta` si le CLI le découpe), et **pas** une entrée
    utilisateur parasite.
- Tests : `compact_boundary_camel_case`, `compact_boundary_snake_case`,
  `status_compacting_emits_in_progress`, `compact_summary_is_not_a_user_turn`,
  `two_boundaries_in_one_run_get_two_ids`,
  `old_wire_without_compact_fields_still_parses`.

**Piège :** ne pas confondre un `system/status` d'un autre type avec une
compaction ; ne pas émettre `Completed` sans frontière.

#### 3.2 Codex — `crates/harness/src/codex/`

**Signal :** notification `thread/compacted` (après coup, sans résumé).

**Changement :**
- `normalize.rs` : `compacted_event()` émet **les deux** : le
  `ContextWindow` actuel (jauge) et un `ContextCompaction { status: Completed,
  trigger: None, tokens_after: None }` ; le prochain `ContextWindow` remplit
  la jauge, le doc garde `tokens_after` absent (pas d'invention).
- `mod.rs` : conserver l'arm `thread/compacted` ; si le payload porte une raison
  ou des tokens, les transmettre.
- Tests : `thread_compacted_emits_boundary_and_stats`,
  `compaction_without_params_still_completes`.

#### 3.3 Cursor — `crates/harness/src/cursor/`

**Signal :** le shim mappe déjà `summary-completed` sur
`{"ev":"compact","reason":"summarized"}`. Le SDK mentionne aussi
`summary-started` (aujourd'hui sans consommateur).

**Changement :**
- `shim.mjs` : `case "summary-started": out({ev:"compact", phase:"started"})` ;
  `summary-completed` → `{ev:"compact", phase:"completed", reason:"summarized"}`.
- `mod.rs` : l'arm `"compact"` émet `ContextCompaction` (`InProgress` si
  `phase == "started"`, sinon `Completed`) en plus des stats actuelles.
- Tests : `compact_frame_records_a_summarize` étendu aux deux phases ; shim
  testé par parsing de frames (pas de dépendance Node dans les tests Rust).

**Piège :** si le SDK n'émet pas `summary-started` sur la version épinglée,
l'état `InProgress` ne s'affiche pas ; la matrice reste `BoundaryOnly` et le
test doit documenter ce cas.

#### 3.4 Famille ACP — chemin partagé (Grok, OpenCode, Qoder, Devin, Copilot, Hermes, Pi, Cline)

**Signal :** `session/update` `compaction_update` et
`compaction_summary_chunk` (RFD ACP Session Compaction, **brouillon**), si
l'adaptateur les implémente et si le client a annoncé
`clientCapabilities.session.compaction` (Gate B). Ces huit agents (Qoder
inclus) partagent le même normalizer : une implémentation, une fixture, pas
huit variantes.

**Changement :**
- `normalize.rs` (`map_update`) : deux kinds.
  - `compaction_update` → `ContextCompaction { id: compactionId, status,
    summary, ... }` ; statut inconnu → `InProgress`, sans inférence de cycle
    de vie.
  - `compaction_summary_chunk` → `ContextCompaction { id, summary_delta }`,
    sans inventer de statut si le chunk arrive seul.
- Kind inconnu : `tracing::debug!` catégorisé + test
  `unknown_update_kind_is_logged_and_dropped` (pas de no-op muet).
- Fixture unique `acp_compaction_update` (`in_progress` → `completed` avec
  chunks), déclinée par `HarnessId` dans le test de surface.
- **Vérification par agent différée** : tableau de suivi dans
  `docs/research/acp.md` (binaire, version épinglée, update observé ou absent),
  Qoder inclus. Tant qu'un agent n'est pas vérifié, sa surface reste `None` —
  jamais une promesse non testée. La validation d'un agent n'élargit pas
  l'allowlist de capacités aux autres. OpenCode n'a plus de
  `session.compacted` public ; l'ACP est la seule voie. `pi-acp` est
  communautaire et épinglé : vérifier sa version avant
  `LifecycleWithSummary`, sinon `Lifecycle`.

#### 3.5 Antigravity — `crates/harness/src/antigravity/`

**Signal :** inconnu. Le protocole NDJSON n'expose aujourd'hui que les steps,
les tools, les todos et `ask_question`.

**Changement :**
- [ ] **Step 1:** Enregistrer un run long (contexte > seuil) et chercher dans le
  NDJSON un signal de compaction/truncation/summarize. Consigner le résultat
  dans `docs/research/antigravity.md` (à créer ; sinon compléter
  `docs/research/harness.md`).
- [ ] **Step 2:** Si un signal existe, l'ajouter à `normalize.rs` (même modèle
  que les autres) et déclarer `Lifecycle` ou `BoundaryOnly`.
- [ ] **Step 3:** Sinon, déclarer `CompactionSurface::None` et **ne pas**
  afficher de ligne. L'absence est honnête ; un faux événement ne l'est pas.

#### 3.6 Muse — `crates/harness/src/muse/`

**Signal :** item `compaction` (aujourd'hui masqué par
`muse/normalize.rs` et le test `housekeeping_items_are_not_shown`).

**Changement :**
- `normalize.rs` : ajouter `"compaction" => self.compaction(item)` ; extraire
  `revision`/`status`, `text` ou `summary` si présents, tokens si présents.
  Émettre `ContextCompaction` (`InProgress`/`Completed`/`Failed`).
- Remplacer le test `housekeeping_items_are_not_shown` par
  `compaction_items_are_shown` et garder `userMessage`/`reminderChild` masqués.
- Tests : item compaction minimal, compaction avec résumé, re-send de la même
  révision (pas de doublon).

#### 3.7 Mock — `crates/harness/src/mock.rs`

- Ajouter un scénario scripté `compaction`: `InProgress` → `Completed` avec
  résumé, puis `Failed` pour un second cas.
- Ajouter aussi `subagent_nested` (`A → B`, outils et texte imbriqués) : il est
  consommé par les Tasks 4 et 5 et par la vérification manuelle de §4, alors
  qu'aucun scénario mock n'existe aujourd'hui.
- Ces scénarios alimentent les tests engine/UI et `scripts/dev-demo.sh` (option
  `--scenario compaction|subagent_nested` ou variable d'environnement mock).
- Test : `mock_compaction_scenario_is_deterministic` et
  `mock_subagent_nested_scenario_is_deterministic`.

**Statut PR1 (3.1, 3.2, 3.3, 3.6 — implémenté 2026-10-29) et écarts assumés :**

`cargo test -p komet-harness` : 211 tests de lib et toutes les suites
d'intégration verts. Les quatre adaptateurs produisent maintenant
`AgentEvent::ContextCompaction`, et la table `capabilities::interaction_surface`
dit vrai pour eux sans y toucher.

- **3.1 Claude** : `SystemFrame.compact_metadata` (alias `compactMetadata`,
  `preTokens`) et `MessageFrame.is_compact_summary` (alias `isCompactSummary`) ;
  `status` était déjà porté par la trame. Le cycle vit dans
  `Normalizer::compaction` avec un drapeau `settled` : c'est ce qui distingue
  « la frontière ferme le cycle ouvert » de « une seconde frontière est une
  seconde compaction ». Les six tests nommés du plan sont écrits, plus
  `boundary_lands_on_the_open_cycle`. **Écart** : la forme réelle de
  `compact_boundary` n'est **pas** vérifiée sur le CLI live (
  `docs/research/slash-commands-inventory.md` — sonde faite sur une session vide) ;
  d'où la tolérance des deux orthographes et le fait qu'une frontière sans
  métadonnée ferme déjà le cycle (`trigger`/`tokens_before` restent inconnus).
  Un `isCompactSummary` marqué `parent_tool_use_id` est **jeté avec un
  `tracing::debug!`** : le résumé d'un sous-agent n'est pas le cycle du parent,
  et le lire comme un steer réveillerait une chip déjà réglée.
- **3.2 Codex** : `compacted_event()` devient `compacted_events(params)` et rend
  **deux** événements (jauge + frontière, id neuf à chaque cycle). La jauge garde
  `thread compacted` comme libellé quand la trame ne dit rien ; `trigger` et
  `preTokens` sont transmis quand ils existent. `tokens_after` reste `None`.
- **3.3 Cursor** : **écart sur le `phase:"started"` demandé**. `shim.mjs` n'est
  pas modifié : `map_shim_frame` est sans état, donc un `started` ne pourrait
  jamais être relié à sa `completed` et la ligne pulserait indéfiniment. La
  matrice dit `BoundaryOnly` pour ce harness — c'est désormais exactement ce que
  l'adaptateur émet : la trame `compact` produit la jauge **et** la frontière,
  avec `trigger` = le propre mot du shim (`summarized`), jamais un `auto`
  inventé. `compact_frame_records_a_summarize` est étendu aux deux événements et
  vérifie que deux résumés portent deux ids.
- **3.6 Muse** : l'item `compaction` n'est plus masqué (`"compaction" =>
  self.compaction(id, item)`), et le test `housekeeping_items_are_not_shown` ne
  garde que `userMessage`/`reminderChild`. Le statut suit `status` de l'item
  (`completed`/`failed`/`cancelled`, tout le reste = `inProgress` — une orthographe
  inconnue n'est pas un accompli) ; l'erreur lit `error` ou `error.message` ;
  les tokens et le `trigger` sont transmis s'ils existent. Le `revision`-gard
  existant empêche déjà le doublon sur re-send. **Ajout hors plan** : les
  `item/delta` d'un item `compaction` sont routés vers le `summary` du cycle et
  non vers le texte de l'assistant — sinon le résumé du fil apparaissait comme
  une réponse du modèle ; l'item sème `emitted` pour qu'un delta qui suit une
  re-send complète s'y ajoute au lieu de la remplacer.

**Statut PR2 (3.4, 3.5, 3.7 — implémenté 2026-10-29) et écarts assumés :**

`cargo test -p komet-harness` (lib + toutes les suites d'intégration) et
`cargo test -p komet-engine --lib` (223 tests) sont verts, `cargo clippy -p
komet-harness -p komet-engine --all-targets -- -D warnings` aussi.

- **3.4 ACP** : les deux kinds du RFD brouillon sont mappés dans `map_update`
  (`compaction_update` → cycle complet, `compaction_summary_chunk` →
  `summary_delta`), un `compaction_update` sans `compactionId` est jeté avec un
  `tracing::debug!`, et le bras `_` journalise désormais tout kind non mappé —
  il n'existe plus de no-op muet sur ce fil. Le statut n'est **jamais inféré** :
  `completed/complete/done/success`, `failed/error/errored`,
  `cancelled/canceled`, tout le reste (et l'absence) = `InProgress` — orthographes
  tolérantes parce que le RFD est un brouillon, jamais parce qu'un agent a été
  observé. La fixture est unique (`fake-acp.sh`, scénario `happy`) et partagée
  par les huit agents : `acp_compaction_update_maps_lifecycle_and_chunks` vérifie
  l'ordre du fil (notice `in_progress` → chunk → frontière `completed` avec
  `trigger` « auto » et `tokensBefore` 165 000). **Écart assumé** : la surface
  reste `None` pour les huit agents — le normalizer mappe ce qui arrive, komet
  n'annonce toujours pas `clientCapabilities.session.compaction` (Gate B
  ouverte), et aucune des huit sondes live n'a été faite. Le tableau de suivi
  (binaire, version épinglée, update observé) est créé dans
  `docs/research/acp.md` avec huit lignes « not observed » : c'est là que la
  validation d'un agent se consigne, une ligne à la fois. Le test
  `unknown_update_kind_is_logged_and_dropped` affirme la chute ; komet-harness
  n'a pas d'infra de `Subscriber` de test, donc le « loggé » n'est pas
  asserté — la non-promesse est écrite dans le test lui-même.
- **3.5 Antigravity** : **Step 1 non fait** — il demande un run long (contexte
  au-delà du seuil) sur le CLI réel, ce qui n'est pas possible ici ; rien n'a
  été inventé à sa place. La déclaration reste donc `CompactionSurface::None` (Step 3),
  qui est exactement ce que l'adaptateur émet aujourd'hui : le NDJSON ne porte
  que steps/tools/todos/`ask_question`, et l'absence d'une ligne de compaction
  est honnête tant que le signal n'est pas observé.
- **3.7 Mock** : `mock::compaction_script()` (deux cycles aux ids fixes —
  `mock-cmp-1` `InProgress` → `Completed` avec résumé et `tokens_before`,
  `mock-cmp-2` `InProgress` → `Failed` avec erreur) et
  `mock::subagent_nested_script()` (A depuis le fil parent, B depuis le fil de A,
  un seul saut par wrapper, outils + texte + `Done` par niveau). Les deux sont
  sélectionnés par `KOMET_MOCK_SCENARIO` (`compaction` | `subagent_nested`) via
  `engine::registry::mock_script`, avec le script de démo streaming comme
  `fallback` : une valeur vide ou inconnue ne casse pas le demo. Les ids sont
  fixes parce qu'un scénario re-joué doit produire le même journal —
  `mock_compaction_scenario_is_deterministic` et
  `mock_subagent_nested_scenario_is_deterministic` le vérifient (ce dernier
  assert aussi que B est bien spawné depuis le fil de A, pas depuis la racine).
  **Écart** : `scripts/dev-demo.sh` n'a pas pris d'option `--scenario` ; passer
  la variable d'environnement suffit (`KOMET_MOCK_SCENARIO=compaction
  scripts/dev-demo.sh`).


---

### Task 4 — Sous-agents : projection du journal de run

**Le trou principal.** Les harnesses émettent déjà `AgentEvent::Subagent`
(taggé `parent_tool_use_id`) et l'engine **journalise déjà chaque événement**
(`sessions.rs:1504`), y compris les événements imbriqués. Aucun consommateur
n'existe : le fold parent ne garde que le statut de la chip.

**Décision : pas de doc enfant.** Le contenu d'un sous-agent ne se synchronise
pas et n'a pas besoin de CRDT. Un doc Loro par sous-agent (récursion, TTL,
RPC, cycle de vie) serait plus lourd que le besoin. On projette le journal
existant à la demande.

**Files:**
- Modify: `crates/engine/src/run_journal.rs` (`subagent_events(chat_id,
  parent_tool_use_id)` : parcourt les lignes, filtre les wrappers `Subagent`
  et **récursive** dans les wrappers imbriqués, préserve l'ordre du journal).
- Modify: `crates/engine/src/sessions.rs` (compteur live par sous-agent pour
  `WatchActivity` ; écriture terminale unique du statut/durée/nb d'outils dans
  le doc parent ; aucun autre changement de route).
- Modify: `crates/engine/src/doc_host.rs` (abandon au restart : un sous-agent
  `Running` devient `Failed`, le journal reste lisible).
- Modify: `crates/doc/src/parts.rs` (champs bornés sur `MessagePart::Tool`,
  écrits terminal seulement).
- Modify: `crates/rpc/src/lib.rs` (`WATCH_SUBAGENT`, `WATCH_ACTIVITY`).
- Modify: `crates/proto/src/agent.rs` (payloads typés `WatchActivity`,
  `SubagentActivity`, `CompactionSummaryResult`).
- Modify: `crates/engine/src/rpc.rs` (handlers).
- Test: `crates/engine/tests/subagents.rs` (nouveau).

**Design :**
- Clé : `(chat_id, parent_tool_use_id)`. Pas d'id composé, pas de doc, pas de
  TTL. **Correction v3 :** le journal ne suit pas encore la vie du chat —
  `RunJournal::discard` n'est appelé nulle part et `DeleteChat` ne purge que le
  snapshot (`doc_host.rs:1857`). Brancher le discard sur la suppression, la
  rétention et l'import, avec un test de non-résurrection.
- `WATCH_SUBAGENT {chatId, parentToolUseId, afterSeq?}` : réutilise
  `SessionsEngine::subscribe(chat_id, after_seq)` — **abonnement puis replay**,
  jamais l'inverse, pour éviter un trou entre le replay et le live ; les
  doublons sont dédupliqués par `seq`. Un événement imbriqué plus profond
  reste sous son parent direct.
- Host-local par construction : le journal vit sur l'hôte. Un viewer distant
  reçoit un résultat typé `Unavailable`. `GetToolDetail` renvoie aujourd'hui un
  vec vide (« never an error », `sessions.rs:484`) : aligner les deux contrats
  sur un type explicite plutôt que prétendre qu'ils sont déjà identiques.
  Jamais un faux transcript vide.
- Doc parent : la chip reçoit `subagent_status` (existant), puis **une seule
  écriture** terminale de `subagent_duration_ms` et `subagent_tool_count`. Le
  décompte live et la preview passent par `WatchActivity`.
- `WatchActivity {chatId, kind, …}` : type proto explicite
  (`toolProgress`, `subagentStatus`, `compaction`), trames coalescées ~100 ms,
  cap 4 Ko côté UI, **jamais journalisées**. Le décompte live d'un sous-agent
  est tenu en mémoire par l'engine, pas relu du doc.
- Restart : `mark_abandoned` ferme un `Running` en `Failed` ; le nombre
  d'outils est reconstructible depuis le journal, la durée ne l'est pas
  (`None`, journal sans horodatage). Le transcript reste reconstructible.
  Journal absent/rollé → l'UI affiche « transcript non disponible sur cet
  appareil ».
- Les commentaires de `proto/src/agent.rs:1793` et `doc/src/parts.rs:462` qui
  annoncent « le doc du sous-agent » sont corrigés en « projection du
  journal ».

**Steps:**
- [x] **Step 1:** Test `subagent_events_are_journaled_and_filtered` : un run
  avec texte/outils imbriqués ne modifie aucune partie texte du doc parent ;
  la projection retourne ces événements dans l'ordre du journal.
- [x] **Step 2:** Test `nested_subagents_stay_under_their_direct_parent` :
  `A → B` n'apparaît pas dans le transcript de `A` au premier niveau.
- [x] **Step 3:** Test `parent_chip_is_written_once_at_terminal` : zéro
  écriture doc pendant le run, une écriture à `Done`/`Failed`, y compris après
  restart.
- [x] **Step 4:** Test `watch_subagent_subscribes_then_replays` : les
  événements déjà journalisés arrivent avant les nouveaux, sans doublon
  (`seq`) **et sans trou** si un événement est appendu entre le replay et
  l'abonnement (réutiliser `SessionsEngine::subscribe`).
- [x] **Step 5:** Test `subagent_transcript_without_journal_is_explicit` :
  `Unavailable`, pas de transcript vide ni d'erreur générique.
- [x] **Step 6:** Implémenter la projection et le handler ; vérifier qu'aucun
  contenu enfant n'est écrit deux fois (journal = seule copie).
- [x] **Step 6b:** Test `deleting_a_chat_discards_its_journal` : suppression →
  `RunJournal::discard` appelé et le transcript de sous-agent ne se rouvre pas
  (pas de résurrection par un snapshot ou un replay).
- [x] **Step 6c:** Test `tool_progress_and_activity_are_not_journaled` : un run
  avec `ToolProgress`/`WatchActivity` ne fait croître le journal que des
  faits durables.
- [x] **Step 7:** `cargo test -p komet-engine subagent`.

**Critère :** le transcript imbriqué est complet, live, récursif et
reconstructible après restart, sans doc enfant ni écriture de contenu dans le
doc parent ; le doc parent ne bouge qu'à l'état terminal ; le journal meurt
avec le chat et aucun événement éphémère n'y entre.

**Statut Task 4 (implémenté 2026-10-29) et écarts assumés.**
Projection : `RunJournal::subagent_events` + la règle d'attribution partagée
`project_feed` (un wrapper compte pour son parent **direct**, la récursion
s'arrête là), réutilisée telle quelle par le direct et par le flux live — replay
et live ne peuvent donc pas se contredire sur « qui a parlé ». `WATCH_SUBAGENT`
est forwardable et streamé : en-tête `{status:"ok",replayed:n}` (ou
`{status:"unavailable",reason}`), puis les trames `{seq,phase,event}` non
enveloppées. Les compteurs live vivent dans la boucle de run
(`SubagentFeeds`, en mémoire, purgés avec le run) ; le stream d'activité les
**lit** et ne compte jamais lui-même, sinon deux spectateurs compteraient deux
choses différentes.

Écarts, volontaires :
- **Coalescence** : pas de timer 100 ms pour `subagentStatus`. Les trames sont
  *déclenchées par un changement* (un outil de plus, un cycle qui s'ouvre ou se
  ferme) — une par tool et une par transition, jamais par delta. Le plafond de
  débit est structurel, pas temporel ; le 4 Ko reste une règle d'affichage UI
  (Task 5), le flux ne charrie aucun texte.
- **Kinds `WatchActivity`** : chaque trame porte désormais `kind`
  (`toolProgress`, `subagentStatus`, `compaction`). Les anciennes clés `id`/`text`
  sont conservées à l'identique, donc `transcript.rs:3494` continue de fonctionner
  sans modification ; le rendu `subagentStatus` arrive avec Task 5. La trame
  `compaction` ne porte **que** l'identifiant, le statut et les tailles — jamais
  le recap, qui reste sur `GetCompactionSummary` (journal hôte, opt-in).
- **Restart** : la balayage `fail_running_subagents` passe les chips `running`
  en `failed` (mêmes règles que le balayage de compaction : ce device uniquement,
  idempotent), et n'écrit **aucun** nombre — `subagentDurationMs` reste absent
  plutôt qu'inventé. Le nombre d'outils est reconstruisable depuis le journal mais
  n'est pas re-dérivé au boot : le faire demanderait un scan du journal par cycle
  abandonné pour un bénéfice d'affichage nul (la puce est déjà *failed*).
- **`parent_chip_is_written_once_at_terminal`** teste le pipeline réel
  (zéro nombre pendant le run, une écriture au `Done` de l'enfant, valeurs
  inchangées après la fin du run). La moitié « après restart » est couverte là où
  la logique habite : `interrupted_subagent_is_failed_and_stays_failed`
  (`crates/doc/src/schema.rs`), et non par un redémarrage complet du moteur.
- Le contrat `Unavailable` est testé **des deux côtés** : au niveau engine
  (`subagent_transcript_without_journal_is_explicit`) et au niveau RPC réel
  (`watch_subagent_rpc_answers_from_the_hosts_journal`, via `core.rpc_service()`).
- `discard` couvre désormais le journal **et** le fichier `.resume` : un chat
  supprimé ne transmet pas son budget de résurrection à un chat de même id.

Tests : `crates/engine/tests/subagents.rs` (8), plus les tests unitaires
`run_journal` (4), `rpc` (4 nouveaux) et `doc/schema` (2 nouveaux).

---

### Task 5 — Sous-agents : UI et vérification par agent

**Files:**
- Modify: `crates/ui/src/state.rs` (`subagent_streams`: cache par
  `(chat_id, parent_tool_use_id)`, alimenté par `WatchSubagent` à l'ouverture
  et fermé à la fermeture de l'onglet).
- Modify: `crates/ui/src/shell.rs` (construire `RightSurface::Subagent`,
  map onglet → `(chat_id, tool_call_id)`).
- Modify: `crates/ui/src/transcript.rs` (chip cliquable, statut/durée/compteur
  écrits au terminal, preview depuis `WatchActivity`).
- Add: `crates/ui/src/subagent.rs` (transcript read-only réutilisant
  `rows_for_entry`, avec bandeau « transcript disponible sur l'hôte »).
- Modify: `crates/ui/src/rail.rs` (badge « subagent »).
- Test: `crates/ui` (ouverture d'onglet, preview, nested, unavailable).

`RightSurface::Subagent(u64)` existe déjà mais est inerte (`shell.rs:262`,
aucun `subagent_tabs`, titre `None`) : la tâche consiste à le peupler, pas à
l'inventer.

**Steps:**
- [x] **Step 1:** Cliquer une chip de spawn ouvre l'onglet `Subagent` ; le
  panneau affiche le transcript live (replay + flux) et un en-tête
  modèle/statut/durée. *(la cible de clic est le **badge** de la chip, pas son
  en-tête — l'en-tête garde son comportement existant : déplier Input/Output.)*
- [x] **Step 2:** Une chip imbriquée ouvre un second onglet (récursion), sans
  mélanger les niveaux.
- [x] **Step 3:** La chip parente affiche « Running · 3 outils · 12 s » via
  `WatchActivity` ; à la fin, « Done · 3 outils · 41 s » depuis la valeur
  terminale du doc. *(v5 : le badge de cycle de vie `agent running/done/failed`
  est déjà rendu depuis `subagent_status`, fingerprinté dans la ligne ; le
  compteur et la durée restent à brancher sur `WatchActivity`.)*
  *(fait : compteur, durée et nom de l'outil courant viennent de la trame hôte ;
  le mot de cycle de vie vient du **doc**, qui seul peut dire « fini » — voir le
  statut Task 5.)*
- [x] **Step 4:** `WatchActivity` fournit une preview d'une ligne (dernier outil
  ou dernier texte), sans écriture dans le doc parent. *(la trame
  `subagentStatus` porte le **nom** du dernier outil, jamais ses arguments ; le
  texte, lui, arrive par `WatchSubagent`, qui est déjà le transcript live.)*
- [x] **Step 5:** Sur un viewer distant (journal absent), l'onglet affiche le
  statut terminal et « transcript non disponible sur cet appareil » — jamais
  un faux vide.
- [ ] **Step 6:** Vérification par agent, **différée et suivie** (une ligne
  cochée = fixture + run réel) :

| Agent | Émission actuelle | Test à ajouter |
| --- | --- | --- |
| Claude Code | `Subagent` taggé (`parent_tool_use_id`, `task_*`) | fixture `claude_subagent_nested` |
| Codex | threads enfants (`ChildRoute`) | fixture `codex_child_thread` |
| Cursor | `taskUpdate` taggé | fixture `cursor_task_update` |
| Grok | tracker + tail `chat_history.jsonl` | fixture `grok_subagent_tail` |
| OpenCode | sessions imbriquées | fixture `opencode_nested_session` |
| Qoder | ACP (émission à vérifier) | fixture `qoder_nested_session` ou `None` déclaré |
| Devin / Copilot / Hermes / Pi / Cline | dépend de l'adaptateur | test de surface : `None` si aucun événement |
| Muse | item `subagent_spawn` | fixture `muse_subagent` |
| Antigravity | aucun | déclaration `None`, pas d'affordance |
| Mock | taggé récursif | scénario `subagent_nested` |

- [x] **Step 7:** `cargo test -p komet-ui && cargo test -p komet-engine subagent`.

**Critère :** on peut suivre un sous-agent en direct et rouvrir son transcript
après coup sur l'hôte ; sur un agent qui n'en émet pas, aucune affordance
n'apparaît ; sur un viewer distant, le statut est visible et l'absence de
transcript est annoncée.

**Statut Task 5 (implémenté 2026-10-29) et écarts assumés.**
`crates/ui/src/subagent.rs` est le panneau : une seule surface de lecture, qui
ne possède aucun contenu — replay puis live via `WatchSubagent`, compteurs via
`WatchActivity`, et le rendu passe par les helpers **déjà** utilisés par le
transcript parent (`rows_for_entry`, `parse_for_row`, `chip_header_row`,
`detail_section`/`detail_body`, `question_row`/`permission_row`/`error_chip`/
`compaction_line`, `komet_doc::fold_event_into_parts`), dont on a seulement
élargi la visibilité. Un texte d'enfant se lit donc exactement comme le même
texte d'adulte — secrets masqués compris — et il n'existe pas de seconde
grammaire de rendu à faire diverger.

Écarts, volontaires :
- **Pas de `subagent_streams` dans `state.rs`** : le tampon vit avec l'entité
  `SubagentPanel`. Les `Task` de souscription meurent quand l'onglet se ferme,
  ce qui est précisément la sémantique voulue (« la fermeture de l'onglet arrête
  le flux ») ; une map dans `AppState` aurait gardé des flux vivants pour des
  onglets fermés, et il aurait fallu inventer le moment où on les purge.
  `RightSurface::Subagent(u64)` reste `Copy` : le handle est alloué par le shell
  (`subagent_seq`), comme `diff_seq`, et pointe vers l'entité.
- **Clic = badge, pas en-tête** : l'en-tête de chip garde son affordance
  existante (déplier Input/Output). Le badge de statut devient la cible qui
  ouvre l'onglet, avec `stop_propagation`. Deux gestes distincts sur la même
  ligne, aucun vol de comportement.
- **Prime au doc pour le mot de cycle de vie** : `subagent_status_label` lit le
  cycle sur le chip (`status.or(live)`) et non sur la trame live. Une trame est
  un instant qui cesse d'être rafraîchi ; la dernière avant le `Done` dirait «
  agent running » pour toujours si elle gagnait. Les nombres, eux, viennent de
  la trame *tant que* le doc dit `Running`, puis de l'écriture terminale du doc —
  jamais l'un ne remplit l'autre : un nombre jamais mesuré reste tu plutôt
  qu'affiché `0`.
- **`lastTool` = libellé, jamais arguments** : une ligne de commande peut porter
  un secret ; les arguments sont à un clic, dans l'onglet, sur l'hôte qui les a
  déjà journalisés.
- **Aucune extrapolation d'horloge** : `elapsedMs` est l'horloge de l'hôte et
  n'avance que quand l'hôte émet. Un viewer qui voit « 12 s » et ne reçoit rien
  pendant trente minutes lit toujours « 12 s » — faux, mais faux *sûrement*, et
  pas une horloge locale qui prétend mesurer le travail d'une autre machine.
- **Rail** : `RailTick.subagents` compte les spawns de la **section** du prompt
  (jusqu'au prompt suivant), marque le tick d'un point et nomme le compte dans
  la carte de survol. La porte d'entrée est `call.is_subagent_spawn()` — la même
  que celle des chips — donc un `Run` ordinaire ne marque jamais un tour.
- **Vérification par agent (Step 6) non faite** : elle demande des runs réels
  (Claude Code, Codex, Cursor, Grok, OpenCode, Qoder, Muse…). Rien n'a été
  simulé pour cocher la ligne ; le mock (`crates/harness/src/mock.rs`, scénario
  `subagent check`, récursif et taggé) exerce le pipeline de bout en bout, pas la
  réalité de chaque adaptateur.

Tests : `crates/ui/src/subagent.rs` (10 — labels live/terminal, aucune
invention de nombre, secondes arrondies, filtrage par `kind`/`parent`, segments
et `streaming` du fold), `crates/ui/src/rail.rs` (2 nouveaux : marque de
délégation, absence de marque sur outil ordinaire), plus les 5 tests engine
`subagent` de Task 4 qui couvrent le contrat côté hôte.

---

### Task 6 — Questions : modèle, ACP elicitation, natifs

**Files:**
- Modify: `crates/proto/src/agent.rs` (`UserInputQuestion` étendu,
  `QuestionSource`).
- Modify: `crates/harness/src/acp/mod.rs` (`elicitation/create` dans
  `handle_server_request_live` ; réponse `accept`/`decline`/`cancel`).
- Modify: `crates/harness/src/acp/normalize.rs` (mapping JSON Schema → questions).
- Modify: `crates/harness/src/claude/mod.rs` (AskUserQuestion : free-text,
  multi-select, « Autre »).
- Modify: `crates/harness/src/muse/` (aligner `request_user_input`).
- Modify: `crates/harness/src/antigravity/mod.rs` (réponse ou refus explicite).
- Modify: `crates/harness/src/cursor/` (déclaration `None`, documentation).
- Modify: `crates/harness/src/mock.rs` (scénario `question_multi`).
- Modify: `crates/ui/src/composer.rs` (carte inline + dock non bloquant).
- Test: `crates/harness/tests/acp.rs` (fixtures elicitation),
  `crates/harness/tests/claude.rs`, `crates/ui`.

**Gate B.** La branche `elicitation/create` n'est active que si
`KOMET_ACP_ELICITATION=form` **et** que l'annonce a été validée sur un agent
réel. Par défaut, les agents ACP continuent d'utiliser le canal
« permission-shaped » actuel : on ne change pas leur comportement en douce.
Le code de mapping est livré derrière le flag, testé en fixtures, mais la
surface par défaut reste `PermissionShaped` jusqu'à la validation.

**Règles de mapping elicitation (form) :**
- `mode: "form"` uniquement ; `url` déclaré mais non supporté dans cette
  itération → refus explicite (`-32602` ou notice UI), jamais un fallback
  silencieux.
- `oneOf`/`anyOf` → options + descriptions ; `enum` → options ; `array`
  d'enum → multi-select ; `boolean` → confirm ; `string`/`number`/`integer` →
  texte/nombre, avec `pattern`, `min/max`, `default`. `format: "password"` →
  `secret: true`. Une réponse libre est capée (même limite que les sorties
  d'outil, 4 Ko) avant d'entrer dans le doc.
- Plusieurs propriétés → plusieurs questions paginées (le wizard existant sait
  déjà paginer).
- Réponse : `accept` avec `content` validé, `decline`, `cancel` ; mapper
  `decline`/`cancel` sur `Deny`/annulation de la question, jamais sur un Allow.
- Un schéma inconnu ou non supporté est **préservé** dans la partie et rendu
  comme un refus explicite, pas comme un formulaire approximatif.

**Steps:**
- [x] **Step 1:** Fixture `elicitation_form_single_select` → une question, deux
  options, une réponse acceptée.
  *(`fake-acp.sh` : `scenario:elicit-single`, enum + `enumNames` + `default` ;
  `acp_elicitation.rs` vérifie options, header, défaut et round-trip
  `"env":"prod"`.)*
- [x] **Step 2:** Fixture `elicitation_form_multi_field` → pages, `required`,
  `default`.
  *(`scenario:elicit-multi` : deux propriétés → deux questions ; le test
  exige les deux réponses.)*
- [x] **Step 3:** Fixture `elicitation_multi_select_array` → labels multiples.
  *(`scenario:elicit-array` : `multi_select` exposé, labels rendus en valeurs
  enum dans l'ordre du schéma.)*
- [x] **Step 4:** Fixture `elicitation_decline_and_cancel` → pas d'Allow,
  partie résolue avec la bonne issue.
  *(`scenario:elicit-cancel` (aucune réponse) et `elicit-decline` (champ
  requis vide) : le wire reçoit `cancel`/`decline`, jamais `accept` ; la
  porte fermée répond `-32601` et le mode `url` `-32602`.)*
- [x] **Step 4b:** Test `input_requested_carries_source` : une question
  elicitation se relit avec `source: Elicitation` ; le fallback
  permission-shaped avec `PermissionShaped` ; un vieux wire avec `Native`.
  *(Fait : les trois sources passent par le fold, le doc et le wire ;
  `source` absent du JSON natif.)*
- [x] **Step 4c:** Test `secret_answer_is_capped_and_not_exported` : une
  réponse libre `secret` est capée à 4 Ko, masquée dans l'UI et absente de
  l'export, mais reste disponible pour l'agent.
  *(Fait : le writer unique `SessionDoc::record_input_response` masque le
  secret (`[withheld: secret answer]`, labels compris) et cape le texte
  libre ; `respond_input` n'expurge que la copie journal/broadcast — le
  resolver du harness reçoit la réponse entière ; `respond_input_prompt`
  (run mort) masque aussi, car ce texte devient un message synchro ;
  l'export utilise la même constante.)*
- [x] **Step 4d:** Scénario mock `question_multi` : deux questions paginées,
  multi-select, champ « Autre », utilisé par `scripts/dev-demo.sh` et les
  tests UI.
  *(`KOMET_MOCK_QUESTION=multi` ; le script existant est nommé et testé
  (`mock_question_multi_scenario_has_pages_checks_and_free_text`) ;
  `dev-demo.sh --questions` l'active et chaque run demande les deux pages.)*
- [x] **Step 5:** Antigravity : soit le protocole accepte une réponse au tool
  `ask_question` (l'envoyer), soit retirer le bridge et déclarer `None` + notice
  dans le fil. Supprimer `let _ = rx.await;`.
  *(`agy -p` est un flux NDJSON one-shot : aucune voie de réponse. Bridge
  retiré (plus de `let _ = rx.await`) ; une `ask_question` devient une
  notice visible dans le fil (part d'erreur explicite), la surface passe à
  `QuestionSurface::None`, et un test fixture
  (`ask_question_is_a_visible_notice_not_an_unanswerable_panel`) vérifie
  qu'aucun panneau n'est ouvert et que le tool reste visible.)*
- [x] **Step 6:** Cursor : conserver `askQuestion` désactivé mais l'exposer dans
  la matrice et l'UI (« Ce harness ne peut pas poser de question »).
  *(`shim.mjs` garde `askQuestion` dans `disallowedTools` (pas de canal de
  réponse SDK) ; `capabilities.rs` déclare `None`, la ligne Golden le fige,
  et `surface_phrases` affiche « Questions: not supported » — testé
  (`harnesses.rs`, ligne Cursor).*
- [x] **Step 7:** UI : la carte de question vit dans le fil (passive,
  historique) + un dock au-dessus du composer (interactif) ; une seule
  affordance active, pas deux formulaires concurrents. Le composer reste
  actif (steer/texte). Clavier 1–9, Entrée, Échap = annuler — vérifier que
  cet Échap ne capture pas le « stop » existant du composer
  (`escape_stop_armed_at`). Multi-select avec coches. Champ « Autre » si
  `allow_free_text`.
  *(Fait : `render_wizard` est un dock inséré AU-DESSUS du pill — plus aucun
  early-return ne remplace le composer ; focus, brouillon, steer et bouton
  d'envoi restent ceux du prompt (`submit_text`/`submit_edit`/`on_submit`
  ne testent plus `wizard`). Le dock possède son propre `ComposerInput`
  (`enable_escape_cancel`) : Échap y émet `Cancelled` → réponse vide
  (decline/cancel, jamais un Allow) sans armer le « stop » ; Échap dans le
  prompt arme toujours le stop 3 s (le dock est un frère du pill, pas un
  ancêtre). Coches multi-select, descriptions d'options, « Other »
  seulement si `allow_free_text`, mention « Sensitive » pour un champ
  secret, badge `+N queued`, bouton Cancel ; le fil garde la carte passive
  (`QuestionRow`).)*
- [x] **Step 8:** Tests UI : deux questions en attente s'affichent dans l'ordre,
  on peut taper pendant ce temps, et une réponse optimiste se réconcilie avec
  la frame doc.
  *(`every_open_request_is_enumerated_in_doc_order` +
  `optimistic_answer_releases_the_queue_before_the_doc_frame` : ordre doc
  sur plusieurs entrées, `pending_input_request_excluding` libère la file
  dès la réponse locale (r2 s'ouvre sans attendre la frame), la frame
  résolue vide le dock ; `wizard_free_text_only_where_the_host_allows_it` et
  `wizard_keeps_a_typed_answer_per_page`. La frappe pendant l'attente est
  structurelle (second input, le prompt n'est plus partagé) — le crate UI
  n'a que des tests purs, donc pas de test d'entité. `composer::tests` :
  52/52.)*

**Critère :** une question ACP standard (multi-champs, multi-select) est rendue,
répondue, persistée et rejouée avec sa `source` ; une réponse `secret` est
masquée et non exportée ; un harness sans canal n'affiche pas de champ.

**Alignements natifs :** Claude `AskUserQuestion` transmet aussi les
descriptions d'options (alignées 1:1 avec `options`, vecteur vide si aucune —
`option_descriptions_align_with_options`,
`question_option_descriptions_empty_without_descriptions`,
`question_multi_select_keeps_free_text`) ; Muse `request_user_input` ouvre le
champ libre (`allow_free_text: true`) puisque `answer()` transportait déjà les
labels hors options en `freeText`/`note` — le parseur est extrait pour être
testé (`questions_parse_with_free_text_ordered_labels_and_selection_mode`).

---

### Task 7 — Plan persistant, progrès d'outils, par agent

**Files:**
- Modify: `crates/harness/src/codex/normalize.rs` et `codex/mod.rs` :
  `turn/plan/updated` → `ToolCall::Todo` sur `LIVE_PLAN_TOOL_ID` +
  `ToolResult` ; `item/plan/delta` ignoré (ou accumulé si aucune mise à jour
  complète) ; `item/commandExecution/outputDelta` → `ToolProgress` throttlé.
- Modify: `crates/harness/src/acp/normalize.rs` : `tool_call_update` avec du
  contenu intermédiaire → `ToolProgress` (pas seulement au statut terminal).
- Modify: `crates/harness/src/muse/normalize.rs` : révisions d'item avec
  `output`/`text` → `ToolProgress`.
- Modify: `crates/engine/src/sessions.rs` : `WatchActivity` transporte
  `ToolProgress` (coalescé, ~100 ms, cap 4 Ko côté UI) via un **chemin
  broadcast-only** ; `publish` ne doit pas journaliser ces trames.
- Add: `crates/doc/src/parts.rs` — `latest_plan(entries)` : dérivation pure,
  dernier `ToolCall::Todo` en ordre de doc, balayage arrière.
- Modify: `crates/ui/src/state.rs` — `active_plan` mis en cache sur
  `apply_transcript`/`apply_transcript_frame` (recalcul seulement si la queue
  du transcript touche un plan ou si la longueur change) et purgé avec le
  transcript.
- Modify: `crates/ui/src/shell.rs` — carte Plan persistante au-dessus du
  composer, dans le même gabarit de largeur que celui-ci ; visible tant qu'il
  reste une tâche ouverte ou qu'un run est live ; disparaît quand tout est
  terminé et que le run est fini.
- Modify: `crates/ui/src/transcript.rs` : la chip ouverte affiche le flux.
- Test: fixtures par agent + test de throttling + tests doc/UI de la carte.

**Par agent :**

| Agent | Plan | Progrès |
| --- | --- | --- |
| Claude Code | `TodoWrite` (déjà) | `None` — ne pas simuler |
| Codex | `turn/plan/updated` (mappé, Step 1) | `commandExecution/outputDelta` (mappé, Step 2, delta → `append: true`) |
| Cursor | `todos` (déjà) | `Final` — le shim SDK ne mappe pas de contenu partiel |
| Grok / OpenCode / Devin / Copilot / Hermes / Pi / Cline | `plan` ACP (déjà) | `tool_call_update` intermédiaire (mappé, Step 3, snapshot → `append: false`) |
| Antigravity | `todos` (déjà) | `Final` — aucune progression mappée sur son fil propre |
| Muse | à vérifier/mapper | `visibleOutput` sur révision `inProgress` (mappé, Step 3) |
| Mock | scénario | scénario |

**Steps:**
- [x] **Step 1:** `codex_plan_update_maps_to_live_todo`. *(Implémenté :
  `normalize::plan_events` mappe `turn/plan/updated` — `plan`/`items`,
  `step`/`text`/`content`, `completed`/`done`, plan vide = effacement — sur
  `LIVE_PLAN_TOOL_ID` + `ToolResult` ; dispatché sur le fil parent. Un
  `turn/plan/updated` d'enfant reste `Consumed` pour ne jamais écraser la
  carte du root ; `item/plan/delta` reste ignoré (les snapshots font foi).
  3 tests verts.)*
- [x] **Step 2:** `codex_output_delta_is_throttled_and_ephemeral` : aucune
  écriture doc **et aucune ligne de journal** (broadcast seul, coalescé).
  *(Implémenté sous trois noms : `output_delta_maps_to_ephemeral_tool_progress`
  (harness, `progress_events` → `ToolProgress { append: true }` sur
  `item/commandExecution/outputDelta` et `item/fileChange/outputDelta`) ;
  `tool_progress_is_broadcast_but_never_journaled` (engine : trame envoyée sur
  le hub broadcast, `seq` 0, zéro ligne de journal, zéro partie doc) ;
  `tool_progress_buffer_coalesces_per_tool_and_caps_the_tail` (engine : une
  fenêtre de 100 ms par outil, `append` accumulé, snapshot remplacé, queue
  bornée à 4 Ko coupée sur frontière UTF-8). Pas de test sur un Codex réel :
  la fenêtre de coalescence est vérifiée sur le buffer, pas sur le fil.)*
- [x] **Step 3:** `acp_intermediate_tool_content_emits_progress`. *(Implémenté :
  `tool_call_update` non terminal avec du contenu → snapshot `ToolProgress`
  (`append: false`) — ACP remplace le tableau de contenu à chaque update —
  et une update `completed` ne produit toujours qu'un `ToolResult`. Muse :
  `visibleOutput` d'une révision `inProgress` → `ToolProgress` plafonné à 16 Ko.
  La plomberie complète est là : variant `AgentEvent::ToolProgress`
  (additif/serde), `doc::parts` ne le plie pas, RPC `WatchActivity`
  (forwardable + stream, `append` retiré du wire car déjà coalescé côté
  moteur).*
- [x] **Step 4:** `latest_plan_returns_the_last_todo_snapshot` : dernier appel
  gagnant, `Some(vec![])` pour un plan effacé, `None` sans plan, et une entrée
  user postérieure ne réinitialise pas la dérivation. *(Implémenté dans
  `komet-doc` : `parts::latest_plan` + 4 tests verts.)*
- [x] **Step 4b:** `active_plan_is_cached_and_purged_with_the_transcript` :
  mise à jour en place pendant le flux, recalcul après un frame qui touche la
  queue, remise à `None` au changement de chat et après troncature. *(Le cache +
  la purge étaient déjà là ; le frame en place est maintenant testé directement
  — `active_plan_follows_in_place_delta_frames` : un upsert qui remplace l'entrée
  live au même rang (longueur inchangée) fait bien bouger `active_plan`, et une
  trame qui retire l'entrée plan la remet à `None` par le chemin « longueur
  changée » ; `active_plan_clears_when_its_entry_is_truncated` couvre la
  troncature, `active_plan_tracks_the_latest_todo_call` le snapshot complet.)*
- [x] **Step 4c:** `persistent_plan_card_shows_unfinished_work` : une liste
  avec une tâche ouverte rend la carte au-dessus du composer, avec
  `terminées/total` et les lignes `✓`/`○` ; la carte ne vole pas le focus et
  ne bloque ni la frappe ni le scroll. *(La règle est testée —
  `persistent_plan_card_shows_unfinished_work` : une tâche ouverte suffit, y
  compris run réglé, et le plafond de 6 lignes annonce les autres (`+N`) au lieu
  de les couper. Le `terminées/total`, les glyphes et le fait que la carte ne
  prend jamais le focus sont du rendu (`shell.rs`, conteneur `div` sans
  `key_bindings` ni `track_mouse`), vérifiés à l'œil, pas par test.)*
- [x] **Step 4d:** `persistent_plan_card_leaves_when_the_plan_is_done` : tout
  terminé et run fini → carte masquée ; run encore live → carte conservée ;
  liste vide → masquée ; les tâches au-delà du plafond d'affichage sont
  annoncées (`+N`), jamais silencieusement coupées. *(Les quatre cas sont testés
  via `plan_card_layout`, extrait du rendu pour ça. Reste non testé : le fait
  que la carte disparaisse visuellement sans déplacer le composer — passage en
  `when_some`, donc hauteur 0, mais non mesuré.)*
- [x] **Step 5:** UI : le flux n'apparaît que dans la chip ouverte ; le
  transcript ne re-rend pas. *(Implémenté dans `transcript.rs` : le watch
  `WatchActivity` suit le cycle de vie du watch de reasoning —
  `maybe_start_activity`/`stop_activity`, `targetDeviceId` pour un chat distant,
  budget de 3 re-tentatives car un relais plus ancien n'a tout simplement pas
  la méthode. Le texte arrive dans `tool_progress` (clé = `part_id` d'outil),
  résolu à chaque paint comme les blobs du sidecar : une trame est un
  `cx.notify`, jamais une reconstruction de ligne, donc les fingerprint du
  cache de lignes ne bougent pas. Section « Live output » rendue uniquement
  sous `open && action_is_open`, hauteur incluse dans le `open_h` analytique de
  la chip et dans celui du fold de groupe ; un outil résolu ne montre jamais sa
  progression (son output vient du doc, le flux ne ferait que le dupliquer) ;
  les entrées dont l'outil est résolu ou sorti du transcript sont purgées au
  `sync`. `live_output_detail` borne l'affichage aux 12 dernières lignes avec
  une ligne « … N earlier lines » — vérifié par
  `live_output_detail_shows_the_tail_under_a_bounded_height`, qui contrôle aussi
  que la hauteur ne dépend pas de la longueur du flux.)*
- [x] **Step 6:** `cargo test -p komet-harness --test acp && cargo test -p
  komet-doc && cargo test -p komet-ui`. *(2026-10-29 : 44 pass / 1 ignored (ACP),
  93 + 1 pass (doc), 607 pass (UI) ; `cargo check --workspace --all-targets`
  clean, `cargo fmt --all` appliqué. Deux `warning`s préexistants non liés :
  `drive_run` `too_many_arguments`, variante `SpacesMenuPage::Projects` morte.)*

**Critère :** ce que le CLI affiche en direct (plan, stdout partiel) est visible
dans Komet quand le protocole l'expose ; sinon la matrice dit `None`/`Final`.
Le plan courant est **toujours** présent au-dessus du composer tant qu'il
reste du travail, quelle que soit la longueur du transcript ou le harness.

---

### Task 8 — Fluidité de l'interaction

**Gate A (ouverte).** La branche fluidité est mergée (#68, `09b04ed`) :
`perf.rs` et `frame-budget.sh` sont sur `main`. Les compteurs existants
couvrent `Transcript`, `Composer`, `Changes` — pas `ContextUsage` ni le rail, et
aucun compteur par ligne. Step 0 comble ce trou avant toute mesure.

**Files:**
- Modify: `crates/ui/src/perf.rs` (`ChildView::{ContextUsage, Rail}` ;
  compteur de rendu par ligne, clé `(kind_tag, id)`). Le collecteur passe de
  `[u32; 3]` indexé par `ChildView` à un vecteur/map dimensionné par variante ;
  la map par ligne est vidée à chaque fenêtre et à la disparition d'une ligne.
  `RowKind` n'est pas `Hash` (il porte des `Arc`/`SharedString`) : passer un
  petit tag, pas la valeur entière.
- Modify: `scripts/frame-budget.sh` (colonnes `context_usage_renders`,
  `rail_renders`, `row_renders` et leur affichage/CSV).
- Modify: `crates/ui/src/transcript.rs` (instrumentation `row_frame(kind, id)`
  sous `KOMET_WINDOW_STATS`, zéro coût sinon).
- Modify: `crates/ui/src/composer.rs` (fin du takeover ; dock de demandes).
- Modify: `crates/ui/src/transcript.rs` (ancrage de scroll, cartes inline).
- Modify: `crates/ui/src/shell.rs` (activity rail si retenu).
- Modify: `crates/ui/src/state.rs` (file de demandes, activité live).
- Test: `crates/ui` (composer actif pendant une demande, scroll stable,
  compteurs).

**Steps:**
- [x] **Step 0:** `per_view_and_per_row_counters_count_once_and_leave_neighbours_alone` : une frame avec
  `context_usage` et deux lignes distinctes incrémente chaque compteur une
  fois, et n'incrémente pas les voisins. Les compteurs sont inertes quand
  `KOMET_WINDOW_STATS` est absent (`row_and_child_counters_are_inert_when_the_knob_is_off`), la map par ligne est remise à zéro à chaque
  fenêtre, et `scripts/frame-budget.sh` parse/affiche les nouveaux champs
  (lignes `row frame budget` + top 5 des lignes re-rendues ; les compteurs
  supprimés sont élagués par `rows_live` :
  `removed_rows_are_pruned_at_the_next_frame_boundary`).
- [x] **Step 1:** `composer_stays_usable_while_a_question_is_pending` : on peut
  saisir et envoyer un steer — couvert par le dock de la Task 6 (les voies de
  submission ne sont plus gated par une question ouverte ; tests du dock).
- [x] **Step 2:** `two_pending_requests_are_ordered_and_keyboard_navigable`
  (ordre doc : r1 active, r2 en file ; touches 1-9, 0 et clé morte = Stay,
  `AutoAdvance` puis `advance()` = Done → libère la file).
- [x] **Step 3:** `compaction_row_does_not_move_the_viewport_when_at_bottom` :
  un raffinement de cycle produit un diff `1..2` de même taille, donc le
  chemin `remeasure_items` de `sync()` (garde les tailles et l'ancre), jamais
  le splice structurel ; le scroll ne bouge que sous `if self.pinned`, donc
  un utilisateur remonté n'est jamais re-scrollé.
- [x] **Step 4:** `approval_timeout_shows_a_countdown_then_auto_denied` : le
  watchdog arme `expiresAt` (additif, proto → doc part éphémère, jamais écrit
  en base, projection `None`), la carte de permission affiche
  `Auto-denies in m:ss` puis `Auto-denying…` ; le timer composer (500 ms) ne
  tourne que tant que la demande est ouverte ; l'auto-deny engine était déjà
  testé (`permission_timeout_auto_denies_and_resolves`).
- [ ] **Step 5:** Capture `scripts/frame-budget.sh --seconds 20` avec un
  scénario mock de compaction : `composer = 0`, `changes = 0`,
  `rail = 0`, `context_usage ≤ 1`, et seules la ligne de compaction et la
  jauge se re-rendent ; les compteurs des autres lignes restent à 0. La
  capture doit citer les nouveaux champs, pas seulement les trois anciens.

**Critère :** I3 et I5 sont vrais et **mesurés par vue/ligne** ; aucune
régression du chantier fluidité.

**Notes d'implémentation (Task 8) :**
- Step 5 non exécuté ici : il exige une session graphique (daemon mock via
  `scripts/dev-demo.sh --daemon-only`, puis `KOMET_WINDOW_STATS=1`). Deux
  réserves à vérifier à la capture, plutôt qu'à l'attente naïve : (1) le rail
  est rendu en ligne dans le `Transcript::render`, donc `rail_renders` suit
  `transcript_renders` — `rail = 0` n'est plausible que si la lecture est
  « rail renders only when the transcript renders » ; (2) la `list` du fork
  gpui-component rejoue le processor de chaque ligne visible à chaque rendu du
  Transcript (pas de cache par ligne), donc « les autres lignes restent à 0 »
  n'est vrai qu'entre rendus du Transcript — ce que la capture doit montrer,
  c'est que la frame de compaction ne déclenche **aucun** rendu de
  composer/changes/context_usage au-delà du necessary.
- `row_frame`/`child_frame` n'empilent pas : le temps d'une ligne n'est pas
  ajouté à `children_us` (le guard Transcript le chronomètre déjà), sinon la
  somme serait deux fois le coût réel.
- Recommandation : quand la capture existe, la coller (table + top lignes)
  dans la PR ; c'est la seule preuve mesurable d'I5 pour cette tâche.

---

### Task 9 — Durabilité et multi-device

**Files:**
- Modify: `crates/engine/src/doc_host.rs` (états « run terminé » explicites).
- Modify: `crates/ui/src/composer.rs` / `transcript.rs` (messages).
- Test: `crates/engine/tests/restart_resume.rs`, `crates/engine/tests/device_routing.rs`.

**Steps:**
- [x] **Step 1:** `answering_after_restart_starts_a_new_turn_and_says_so`.
- [x] **Step 2:** `permission_after_restart_is_recorded_but_run_is_gone` — la
  partie affiche « décision enregistrée, run terminé », pas un panneau fantôme.
- [x] **Step 3:** `concurrent_permission_decisions_keep_deny` (règle existante,
  test multi-device).
- [x] **Step 3b:** `concurrent_answers_converge_on_doc_order` (nom réel ; le
  test du plan était déjà écrit à la Task 6 — renforcé ici avec l'idempotence
  par appareil après fusion : une réponse rejouée sur les deux répliques ne
  crée pas une troisième ligne) : deux appareils répondent hors ligne, la
  fusion élit le même gagnant (§1.5) et un seul conflit est affiché. Test
  jumeau `delivered_answer_is_not_the_merge_winner` : le gagnant de position
  (index 0) est la réponse **non** remise ; `applied` reste sur la réponse
  remise par l'hôte et survit au round-trip snapshot.
- [x] **Step 3c:** `stale_question_is_historical_not_actionable` : une question
  déjà résolue est listée comme historique — la réponse tardive d'un autre
  appareil est Rejected « recorded, not delivered », ajoutée comme conflit
  (`applied=false`) et ne dispatche **aucun** run ; la question encore ouverte
  du run crashé reste actionnable et démarre le nouveau tour.
- [x] **Step 4:** `compaction_metadata_survives_a_viewer_reconnect` :
  statut/tokens/trigger visibles partout ; le texte du résumé n'apparaît que
  si `SummaryInDoc`, sinon « disponible sur l'hôte ». `subagent_status`
  terminal visible partout ; le transcript distant affiche l'absence.
- [x] **Step 4b:** `subagent_duration_is_absent_after_crash` : compteur
  reconstruit depuis le journal, durée `None` (pas de valeur inventée) quand
  le run meurt avant le terminal.
- [x] **Step 5:** `cargo test -p komet-engine restart_resume` → 14 passed,
  1 ignored (`real_claude_…`, nécessite le CLI claude authentifié).

**Critère :** I2 est vrai sur les trois surfaces (question, permission,
compaction) ; pour le sous-agent, statut/durée/compteur survivent et l'absence
de transcript distant est annoncée.

**Notes d'implémentation (Task 9) :**
- Aucun code de production nouveau : les mécanismes (fallback « answered as
  new turn », rejet « recorded, not delivered », conteneurs `responses`
  matérialisés à la pousée, `fail_running_subagents`) étaient en place ;
  cette tâche les a **nommés par des tests** (§1.5, Steps 1-4b) et couvert
  l'idempotence multi-répliques qui manquait.
- `device_routing.rs` n'a pas eu besoin de test jumeau : la route « réponse
  d'un pair hors ligne → conflit affiché, jamais rejouée dans le prompt » est
  la commande `RespondInput` déjà testée ici et dans permission_flow.

---

### Task 10 — Connexion fluide et état lisible

**But :** supprimer l'attente muette. Aujourd'hui `ConnectionStatus` n'a que
`Connecting`/`Ready`/`Failed(String)` (`proto/src/view.rs:23`), la reconnexion
est un one-shot à 500 ms (`state.rs:1886`) et un échec laisse l'écran de garde
avec un bouton Retry. Rien ne distingue « moteur qui se reconnecte »,
« harness qui démarre » et « modèle qui réfléchit ».

**Files:**
- Modify: `crates/proto/src/view.rs` (`ConnectionStatus` enrichi :
  `Connecting`, `Reconnecting { attempt, retry_in_ms, reason }`, `Ready`,
  `Degraded { reason }`, `Failed { message, retryable }`). Type non sérialisé,
  donc migration interne autorisée ; mettre à jour tous les `match`.
- Modify: `crates/proto/src/agent.rs` (payload éphémère `RunPhase` dans
  `WatchActivity` : `starting`, `waiting_model`, `streaming`, `tool`).
- Modify: `crates/engine/src/sessions.rs` (émettre `RunPhase` au dispatch,
  après `SessionStarted`, au premier texte/outil ; broadcast-only, jamais
  journalisé, coalescé comme Task 4).
- Modify: `crates/ui/src/state.rs` (boucle de reconnexion avec backoff
  exponentiel + jitter, plafond, compteur de tentatives et prochaine échéance ;
  conservation du transcript et des échos pendant la reconnexion ; consommation
  de `RunPhase`).
- Modify: `crates/ui/src/shell.rs` (carte de garde : compte à rebours et
  « Retry now » ; bandeau discret pendant `Reconnecting`/`Degraded` sans vider
  la frame ; même état pour un environnement appairé).
- Modify: `crates/ui/src/composer.rs` (envoi pendant reconnexion : file locale
  avec raison nommée, jamais un bouton mort ; orbe `Connecting`).
- Modify: `crates/ui/src/transcript.rs` (working trailer : nomme la phase et
  utilise `OrbState::Connecting`/`Working`/`Searching`).
- Test: `crates/ui` (state/reducers, shell), `crates/engine` (phase),
  `crates/engine/tests/restart_resume.rs` (reprise après daemon).

**Design :**
- **Backoff** : 500 ms, 1 s, 2 s, 5 s, puis 10 s, avec jitter de ±20 % ; le
  plafond de tentatives automatiques est explicite (proposition : 6). Au-delà,
  `Failed { retryable: true }` et le bouton Retry. Une tentative manuelle
  remet le compteur à zéro.
- **Continuité** : une reconnexion ne vide ni le transcript ni les échos ; les
  watches sont recréés par `attach_engine` et le transcript est re-souscrit
  (`state.rs:2019`). Un changement de compte/environnement, lui, peut vider —
  c'est un autre contrat, déjà couvert par `prepare_runtime_replacement`.
- **Phases** : `starting` couvre le spawn du CLI ; `waiting_model` couvre
  l'attente du premier token après `SessionStarted` ; `streaming`/`tool` sont
  les phases existantes. Aucune phase n'est inventée si le signal manque : un
  harness qui ne se signale pas reste `starting` avec un timeout visible, puis
  `Degraded { reason: "no output" }` — jamais un spinner éternel.
- **Pas de nouvel état wire durable** : `RunPhase` est éphémère. Un viewer
  distant qui ne le reçoit pas retombe sur l'indicateur `Working` existant.

**Steps:**
- [x] **Step 1:** `connection_backoff_is_bounded_and_visible` (nom réel,
  `crates/ui/src/state.rs`) : la chaîne complète est conduite par
  `next_reconnect_step` (ajouté à `proto::view` à côté du tableau, du plafond
  et du jitter déjà écrits) — 500 ms → 1 s → 2 s → 5 s → 10 s → 10 s,
  **six** tentatives puis `None`, donc `Failed { retryable: true }` et la main
  à l'utilisateur. Le jitter ±20 % est testé borné (800/1200 ms autour de
  1000, et `unit` hors `[0,1]` clampé). L'état pendant l'attente est exposé :
  `ConnectionStatus::Reconnecting { attempt, retry_in_ms, reason }` +
  `AppState::reconnect_deadline`, et `gate_phase` répond `Ready` — pas d'écran
  blanc pendant que la chaîne tourne.
- [x] **Step 2:** `reconnect_keeps_the_last_frame_and_the_echoes`. Le contrat
  est nommé par une fonction — `AppState::drop_engine_subscriptions` — qui ne
  jette QUE les pumps (`watch_tasks`, `data_tasks`, relais d'environnements,
  `transcript_task`) : le test remplit transcript + écho + selected_chat,
  appelle, et vérifie que les deux premiers survivent et que la sélection
  reste. La re-souscription du transcript du chat sélectionné est déjà dans
  `attach_engine` (repris tel quel, commenté là-bas).
- [ ] **Step 3:** Test `send_while_reconnecting_is_queued_with_a_named_reason`
  : le texte part dans la file, l'UI affiche « reconnexion… », aucun message
  perdu, aucun doublon au retour.
- [x] **Step 4:** deux tests nomment la machine à phases côté moteur :
  `phase_change_names_only_what_changed` (une trame par CHANGEMENT — deux
  deltas d'affilée n'émettent qu'une `streaming`, un delta vide n'invente rien,
  un harness muet garde la phase du dispatch) et
  `run_phase_frames_are_broadcast_but_never_journaled` (`seq == 0` sur le hub,
  `latest_seq` inchangé : la journal ne reçoit que les 4 évènements durables).
  Émission : `starting` dès l'entrée dans `drive_run` (avant le spawn du CLI),
  puis la phase déduite de chaque évènement reçu. `is_ephemeral` descends dans
  la liste, `doc::parts` plie `RunPhase` dans la branche vide, et
  `rpc::activity_stream` la diffuse tagged `{"kind":"runPhase"}`.
  Côté reprise : assertion ajoutée à
  `restart_roundtrip_restores_chats_transcript_and_resume` — après deux tours
  et redémarrage, le journal rejoué ne contient AUCUNE trame de phase.
- [x] **Step 5:** `working_trailer_names_the_phase` + la fonction pure
  `phase_trailer(phase, sending, word)` : `starting` → « Starting the harness »
  / `OrbState::Connecting`, `waitingModel` → « Waiting for the model » /
  `Working`, `streaming` → « Answering » / `Working`, `tool` → « Running a tool »
  / `Searching`. `None` garde le mot de saveur d'aujourd'hui (un moteur plus
  ancien, ou un viewer arrivé après la trame, ne tombe jamais sur le vide), et
  `sending` premier : les trames en vol appartiennent au tour précédent.
  Aucune animation ajoutée.
- [x] **Step 6:** `paired_environment_reconnect_uses_the_same_states` autour de
  `AppState::link_status()` : environnement appairé `Connecting` →
  `Connecting`, `Offline` → `Degraded { reason: "<label> is offline" }`,
  `Online` → l'état du moteur lui-même. Le test vérifie aussi que le `Ready`
  local ne peut pas prêter sa santé à un environnement en train d'être composé
  (c'est ainsi que les lignes d'un environnement s'affichaient sous un autre).
  Le bandeau lit `link_status()`, donc un seul vocabulaire et une seule
  implementation pour les deux surfaces.
- [ ] **Step 7:** `cargo test -p komet-ui connection && cargo test -p komet-engine
  run_phase && cargo test -p komet-engine restart_resume`.

**Notes d'implémentation (Task 10) :**
- `Degraded` a un producteur réel : l'environnement appairé hors ligne
  (`link_status`), pas un watchdog de silence. Le design du plan (« harness muet
  → timeout visible puis `Degraded { reason: "no output" }` ») est servi
  autrement : le trailer dit « Starting the harness » avec le chronomètre
  visible, et le moteur ne reçoit AUCUN watchdog de stall — décision déjà
  prise et documentée dans l'en-tête de `sessions.rs` (un agent peut
  légitimement attendre plus longtemps que n'importe quel timeout, et un
  processus vivant EST le signal de travail). Inventer un seuil ici ferait
  passer un agent lent pour un moteur dégradé sur toute l'UI.
- Nommage du fil : le plan écrivait `run_phase` / `waiting_model`, le repo est
  en `tag = "type", rename_all = "camelCase"` → `runPhase` / `waitingModel` sur
  le wire comme pour les autres évènements.
- La chaîne de reconnexion remplace le one-shot à 500 ms ; le garde-fou contre
  deux chaînes parallèles est un champ dédié `reconnect_pending`, pas l'état
  affiché — sinon l'échec d'une tentative ne pourrait jamais faire avancer la
  chaîne au-delà de la tentative 1.
- `retryable: false` (pas de `boot_config`) ne rend plus un bouton « Retry »
  muet : la carte de garde affiche « Engine unreachable » non-cliquable.
- Les trames `runPhase` ne sont jamais relayées comme état durable : un viewer
  distant sans la souscription `WatchActivity` retombe sur `Working` (déjà le
  cas pour `ToolProgress`, Task 4).

**Critère :** aucune attente sans nom, aucun écran blanc pendant une
reconnexion, aucun message perdu pendant une coupure, aucune boucle de
reconnexion infinie ; le démarrage du harness et l'attente du modèle sont
distingués dans l'UI.

---

### Task 11 — Interaction directe au clavier et actions inline

**But :** rendre les actions fréquentes accessibles sans chasse au pixel et
sans changer de surface. La base existe déjà — `send_button_mode`
(`composer.rs:376`) gère Send/Steer/Stop/Queue, la file `prompt_queue` draine
au retour à Idle, `ErrorRetryTurn` existe (`shell.rs:8559`) — mais la file n'est
ni inspectable ni annulable, et les demandes en attente n'ont pas de raccourci
global.

**Files:**
- Modify: `crates/ui/src/composer.rs` (bandeau de file avec annulation ;
  navigation du wizard de question au clavier ; focus rendu au compositeur
  après envoi/résolution).
- Modify: `crates/ui/src/transcript.rs` (actions inline : copier, ouvrir les
  détails d'outil via `GetToolDetail`, relancer un tour échoué via
  `ErrorRetryTurn` ; anneau de focus visible).
- Modify: `crates/ui/src/shell.rs` (actions globales : focus compositeur,
  prochaine demande en attente, stop ; contexte clavier prioritaire).
- Modify: `crates/ui/src/state.rs` (ordre des demandes `actionable` ;
  sélection de la demande ciblée par le raccourci).
- Test: `crates/ui` (reducers + parcours clavier).

**Design :**
- **Raccourcis proposés** (à valider en revue, mais un seul jeu) :
  `mod-shift-c` focus compositeur ; `mod-shift-q` prochaine demande
  (question/permission) ; `mod-.` stop ; `Enter` envoie/steer, `Échap` ferme
  la surface supérieure puis arme le stop — **jamais** une résolution de
  permission. Approuver/refuser restent des actions explicites de la carte,
  atteignables par Tab/Entrée.
- **File visible** : le bandeau « N queued » ouvre la liste, chaque item peut
  être annulé ; annuler le premier item ne bloque pas le drain. Pas de
  réordonnancement dans cette tâche.
- **Actions inline honnêtes** : « Relancer » n'apparaît que sur un tour
  `retryable` ; « Détails » n'apparaît que si `GetToolDetail` est servi ;
  « Copier » est toujours local. Aucune action inventée par harness.
- **Focus** : après envoi, après résolution d'une permission/question et après
  fermeture d'une popover, le focus revient au compositeur ; une notification
  ne vole jamais le focus.

**Steps:**
- [ ] **Step 1:** Test `pending_request_is_reachable_from_anywhere` :
  `mod-shift-q` cible la première demande actionnable dans l'ordre du doc,
  l'amène à l'écran sans sauter le scroll si elle est déjà visible, et pose le
  focus sur son premier contrôle.
- [ ] **Step 2:** Test `keyboard_approves_and_denies_without_a_mouse` :
  Tab/Entrée parcourent les choix ; Entrée approuve le choix primaire, jamais
  un `AllowAlways` implicite.
- [ ] **Step 3:** Test `escape_never_resolves_a_permission` : Échap ferme
  popover → wizard → arme le stop, dans cet ordre ; aucune transition de
  permission n'est émise.
- [ ] **Step 4:** Test `queued_prompt_can_be_cancelled` : annuler le premier
  item le retire et le drain envoie le suivant ; annuler le dernier laisse la
  file vide sans faux « N queued ».
- [ ] **Step 5:** Test `focus_returns_to_composer_after_send_and_resolution`.
- [ ] **Step 6:** Test `retry_turn_is_inline_only_when_retryable` : un échec
  non retryable n'affiche pas « Relancer » ; un échec retryable déclenche
  l'action existante, sans nouvelle RPC.
- [ ] **Step 7:** `cargo test -p komet-ui keyboard && cargo test -p komet-ui
  queue`.

**Critère :** un parcours complet — lire, répondre, approuver, steer, stop,
annuler un prompt en file — se fait au clavier ; aucune action souris-only sur
ces chemins ; Échap ne peut jamais approuver.

---

### Task 12 — Observabilité, documentation, garde-fous

**Files:**
- Modify: `crates/engine/src/sessions.rs` (compteurs par harness :
  reçus/affichés/droppés ; log debug, pas de spam).
- Modify: `crates/harness/src/acp/mod.rs` et `crates/harness/src/claude/mod.rs`
  (auto-allow tracé **hors délégation** : un `PermissionResolved` ou un
  événement dédié, jamais un retour silencieux ; en Full access/Read only c'est
  le niveau qui autorise, cf. I6).
- Modify: `crates/ui/src/settings/harnesses.rs` (surface effective + derniers
  événements observés en mode debug).
- Modify: `ARCHITECTURE.md` §6 (matrice « Interaction surface », même règle que
  le sandbox).
- Modify: `docs/PARITY.md` (corriger la ligne Cursor : SDK shim, `askQuestion`
  non supporté ; ajouter compaction/sous-agents/questions).
- Modify: `docs/research/acp.md` (RFD compaction/elicitation **à l'état de
  brouillon**, version épinglée par agent, capacités annoncées seulement si
  Gate B activée).
- Modify: `docs/research/slash-commands-inventory.md` (statut `/compact`).
- Modify: `docs/chat2-sync.md` (politique : métadonnées de compaction dans le
  doc, résumé journal-only par défaut + opt-in, sous-agent projeté depuis le
  journal, sorties d'outils journal-only).
- Modify: `AGENTS.md` (Known traps : un nouveau harness déclare sa surface
  d'interaction et ajoute ses fixtures).
- Test: `crates/harness/tests/surfaces.rs` (test « no silent drop »).

**Steps:**
- [ ] **Step 1:** Test `no_silent_drop_without_a_surface_entry` : implémenter
  le contrôle par **compteurs de réception/drop par harness** et une fixture
  par famille (twins, méthode inconnue, compaction), pas par scan statique des
  `continue` — un test ne peut pas lire le source de façon fiable. Chaque drop
  doit être couvert par une entrée de matrice ou un log catégorisé.
- [x] **Step 1b:** Test `auto_allow_is_visible_in_the_feed` : l'auto-accept ACP
  (`session/request_permission`) et l'auto-approve Claude produisent un fait
  visible (Permission résolue avec `decided_by` = hôte, ou événement dédié).
  Sinon, retirer « tout auto-allow est tracé » de I6.
  *(2026-10-30 : **retiré de I6, décision assumée** — Full access existe pour
  donner le contrôle total à l'agent : une ligne « auto-alloué » par appel
  noierait le fil sous le bruit de ce que l'utilisateur a explicitement
  délégué. La trace est le niveau lui-même (chip `Read only`/`Sandboxed`/`Full
  access`, `SandboxLevel` porté par la requête de run), pas un fait par appel.
  L'auto-refus de *Read only* suit la même règle ; l'outil refusé reste visible
  par son erreur. Ce qui reste interdit et testé : un allow qui **contredit**
  le niveau (`AllowAlways` implicite hors Full access, deny écrasé par
  `PermissionResolved`) — voir `acp_auto_permission` et la règle deny-wins du
  fold. Aucun test « ligne de feed » n'est donc écrit, et aucun `PermissionResolved`
  synthétique n'est inventé pour un appel que le CLI n'a jamais posé.)*
- [ ] **Step 2:** Golden fixtures anonymisées par agent sous
  `crates/harness/tests/fixtures/` (une par famille de signal).
  *(2026-10-30 : **inventaire fait, captures manquantes** —
  `docs/research/signal-fixtures.md` recense, famille par famille, ce qui
  existe (L1 JSON inline dans le normalizer, L2 script `fake-*.sh|py` lancé
  comme vrai processus, L3 capture live anonymisée) et ce qui manque. État
  réel : compaction prouvée au niveau processus **uniquement pour la famille
  ACP** (`fake-acp.sh` scénario `happy`) ; Claude/Codex/Cursor/Muse et leurs
  plans sont en L1 (seuls les plans ACP sont en L2) ; la progression ACP reste
  L1 car `fake-acp.sh` n'envoie que des updates `completed` ; sous-agents ACP
  n'ont **aucune** fixture processus (unités seulement) ; L3 existe pour Claude
  (`live-2.1.228`) et Codex
  (`live-0.146.1`) mais **seulement** pour les sous-agents ; Gate B : 0/8 agent
  ACP sondé. La case reste ouverte tant qu'aucune capture réelle n'est
  enregistrée — aucune n'a été inventée.)*
- [x] **Step 3:** Mettre à jour les docs ci-dessus (français pour les plans,
  anglais pour `ARCHITECTURE.md`/`PARITY.md` comme le fichier). Documenter :
  le journal est supprimé avec le chat ; un vieux client qui relit
  `kind: "compaction"` le dégrade en `Text` vide (`schema.rs` `_ =>`), donc ne
  le réécrit pas ; les 14 harnesses et l'allowlist Gate B par agent.
  *(2026-10-30 : matrice « Interaction surface » en `ARCHITECTURE.md` §6
  (14 lignes, projetée depuis `capabilities.rs`, Gate B nommée) ; lignes
  Compaction / Sous-agents / Questions dans `docs/PARITY.md` §4 et ligne Cursor
  corrigée (shim `@cursor/sdk`, `askQuestion` sans canal de réponse) ;
  politique doc-vs-journal, discard avec le chat, éphémère non journalisé et
  dégradation d'un vieux lecteur dans `docs/chat2-sync.md` §A4 ; trap
  « surface d'interaction + fixtures » dans `AGENTS.md`. `acp.md` et
  `slash-commands-inventory.md` étaient déjà à jour.)*
- [ ] **Step 4:** `cargo fmt --all && cargo check --workspace --all-targets &&
  cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`.

**Critère :** un nouvel adaptateur ne peut pas shipper sans déclaration ni
fixture ; la doc ne décrit plus un comportement qui n'existe pas.

---

### Task 13 — Barre de widgets du composer (Changes / Sous-agents / Processus)

Trois chips au-dessus du composer, dans un ordre fixe et indépendants :
Changes, Sous-agents, Processus.

| Widget | Apparaît quand | Clic |
| --- | --- | --- |
| **Changes** | le diff du checkout n'est pas vide | ouvre le panneau de droite directement sur Changes, **pas de pop-up** |
| **Sous-agents** | au moins un sous-agent tourne | pop-up collée au-dessus du widget : liste des sous-agents |
| **Processus** | au moins un processus d'arrière-plan tourne | pop-up collée au-dessus du widget : liste des processus |

« Processus en arrière-plan » = ceux lancés par l'**agent** (Claude, Codex,
ACP), pas les shells PTY de l'utilisateur — ceux-ci gardent leur surface
Terminal et ne passent jamais par ces chips.

**Files:**
- Add: `crates/ui/src/composer_widgets/` — `chip.rs` (kit partagé),
  `strip.rs` (hôte de la barre, un `Popup<WidgetKind>` partagé : ouvrir un
  widget ferme l'autre), `changes_chip.rs`, `subagents_chip.rs`,
  `processes_chip.rs`, `popup_card.rs`.
- Modify: `crates/ui/src/shell.rs` — `open_changes_pane()` (ouvre **sans**
  basculer : un clic sur le chip ne ferme jamais le panneau, ne fait rien si
  Changes est déjà actif), `open_subagent_tab(SubagentOpen)`,
  `open_process_tab(id)` ; la barre se rend au bord supérieur du conteneur du
  composer (absolu, 8 px au-dessus, aligné à gauche), au-dessus du dock
  permissions/questions, sans décaler le transcript.
- Modify: `crates/ui/src/state.rs` — un magasin d'activité par chat alimenté
  par **un** abonnement par flux (`WatchCheckoutDiffs`, `WatchActivity`) que
  les chips et les onglets lisent ; une entité séparée par catégorie pour qu'un
  tick de compteur ne re-rende ni le composer ni le Shell.
- Modify (processus) : `crates/proto/src/agent.rs` (`ProcessStarted`,
  `ProcessOutput` avec tampon circulaire côté hôte, `ProcessExited`, trame
  `processStatus` dans `WatchActivity`), `crates/rpc/src/lib.rs`
  (`WatchProcess`, `StopProcess`), `crates/engine/src/sessions.rs` (compteurs
  par processus, purgés avec le run), `crates/harness/*` (émission réelle par
  adaptateur, cf. step 3), `crates/ui/src/process.rs` (`ProcessPanel`, calqué
  sur `SubagentPanel` : en-tête, queue de sortie en lecture seule, état
  « terminé »).
- Modify: `crates/harness/src/capabilities.rs` (`background_processes` dans la
  matrice : un harness sans support n'affiche **jamais** le chip).

**Spec visuelle (commune aux trois chips) :** hauteur 24 px, rayon complet,
bordure 1 px `theme.border`, fond `ink(0.04)` (`0.07` au survol, `0.10` pop-up
ouverte), texte 11,5 px, padding 9 px, écart 6 px entre icône, libellé et
compteurs. `+N` en `theme.success`, `−M` en `theme.danger`. Pastille `busy`
pulsante (`KOMET_PULSE`) pour sous-agents et processus. Pop-up : coin bas-gauche
ancré sur le coin haut-gauche du chip, 6 px d'écart, sans flèche, largeur
340 px, hauteur max 320 px avec scroll, animation `menu-in/out` existante,
fermeture par Échap ou clic extérieur ; un chip qui tombe sous la marge de la
fenêtre se recale à 8 px du bord.

**Détail par widget :**
- **Changes** : libellé `N files +A −D`, compact au-delà de 999 (`1.2k`).
  Visible seulement sur un projet git (comme le bouton de la barre de titre).
  Se masque 600 ms après être retombé à zéro (pas de clignotement pendant un
  commit).
- **Sous-agents** : libellé `N agents`. Pop-up : les sous-agents en cours puis
  les terminés du tour (5 max). Ligne de 36 px = pastille, titre, modèle, et à
  droite `3 tools · 12s` via `subagent_status_label`. Un clic ouvre l'onglet
  `SubagentPanel` (même `SubagentOpen`) et ferme la pop-up. Quand le dernier se
  termine, le chip reste 6 s en « done » puis se replie, sauf pop-up ouverte.
- **Processus** : libellé `N processes`. Ligne = commande en monospace tronquée
  au milieu, durée, dernière ligne de sortie en gris, bouton Stop au survol. Un
  clic ouvre une surface `ProcessPanel`. Un processus qui sort avec un code non
  nul garde le chip visible jusqu'à ce qu'on l'ouvre.

**Steps:**
- [ ] **Step 1:** Kit + barre + Changes. Tests purs : `summarize_patch`,
  `format_count`, machine de visibilité (tableau d'entrées/sorties). Critères :
  clic Changes qui ouvre le panneau sans pop-up ; aucun clignotement au commit ;
  p95 de build inchangé en streaming d'éditions (`frame-budget.sh`).
- [ ] **Step 2:** Sous-agents. Dépend des trames `subagentStatus` (faites,
  Task 4) et de `subagent_status_label` (Task 5). Critères : pop-up collée au
  chip sur 3 positions de fenêtre ; ouvrir un chip ferme l'autre sans
  clignotement.
- [x] **Step 3:** Découverte des processus : inventaire par harness (Claude,
  Codex, ACP) de ce qui est réellement émis, avec une fixture réelle (modèle :
  `live-2.1.228-background-subagent.jsonl`). **Bloqué sur fixture** : ne pas
  figer les événements sur une supposition (Claude Code expose probablement le
  Bash en arrière-plan avec sa sortie, à confirmer).
  *(2026-10-30 : **débloqué sans nouveau run** — la fixture réelle déjà dans
  l'arbre répond. Inventaire dans `docs/research/signal-fixtures.md`
  §« Background processes ». Claude Code : cycle complet
  `background_tasks_changed` + `task_started/progress/updated/notification`,
  `run_in_background: true` sur le call, `output_file` + `TaskOutput`/
  `TaskStop` = output et stop adressables par `task_id`. **Piège mesuré** :
  `task_type: local_bash` se déclenche aussi pour un Bash **au premier plan
  dans un sous-agent** (`owned_by_subagent`), donc ce n'est pas un signal de
  processus d'arrière-plan ; seuls `run_in_background` et `local_agent` le sont.
  Codex : `processId` + `outputDelta` mais aucune vie détachée observée → pas
  d'événement inventé. ACP/Muse/Cursor/Antigravity : aucun signal → chip masqué.)*
- [ ] **Step 4:** Processus : événements, RPC, chip, pop-up, `ProcessPanel`.
  Critère : kill depuis la pop-up reflété dans le panneau.
- [ ] **Step 5:** Finitions : mouvement réduit respecté, scénario de démo dans
  `scripts/dev-demo.sh` (moteur mock) montrant les trois widgets sans agent
  réel, docs mises à jour.

**Critère :** depuis le composer, on voit d'un coup d'œil ce qui tourne
(modifications, sous-agents, processus), un clic mène à la surface qui le
détaille, et un harness sans support n'affiche jamais de chip vide.

---

## 4. Tests et fixtures

- **Harness** : une fixture par **famille de signal** (compaction Claude,
  frontière Codex, compact Cursor, update ACP, item Muse, question, plan,
  progrès) ; les golden fixtures par agent sont ajoutées au fur et à mesure
  des vérifications, pas déclarées d'avance. Les fixtures vivent dans
  `crates/harness/tests/fixtures/` et sont parsées, pas exécutées.
- **Engine** : `subagents.rs` (projection journal, subscribe-puis-replay sans
  trou, durée absente après crash), `restart_resume.rs` (compaction
  interrompue close en `Failed`, une seule fois), `device_routing.rs`,
  `turn_quiesce.rs` (une compaction en cours ne doit pas être lue comme un
  tour terminé), politique de résumé, suppression du journal avec le chat,
  événements éphémères hors journal (`RunPhase` inclus), auto-allow **hors
  délégation** tracé (le niveau Full access/Read only est la trace, pas un fait
  par appel).
- **Doc** : round-trip vieux doc, cap du résumé (UTF-8 et `summary_bytes`),
  concaténation des `summary_delta`, deux compactions dans un run,
  convergence **et** réponse remise (`applied`), `source` de question, entrée
  `System`, export des nouveaux parts et omission des secrets.
- **UI** : rows, file de demandes actionnables vs historiques, onglet
  sous-agent, scroll, compteurs de rendu par vue/ligne, `tokens_after`
  absent, question `secret`, phases de connexion/run, backoff et reprise,
  parcours clavier complet, annulation d'un prompt en file, focus rendu au
  compositeur, Échap sans résolution de permission, carte Plan persistante
  (affichage, progression, disparition, cap `+N`).
- **Journal** : discard à la suppression, rétention, replay/rotation et
  absence explicite (`Unavailable`).
- **Manuel** : `scripts/dev-demo.sh --slow` + scénarios mock
  `compaction`, `subagent_nested`, `question_multi` ; tuer le daemon pendant
  un run pour vérifier reprise sans écran blanc et file d'envoi ; parcours
  clavier complet sans souris. `scripts/frame-budget.sh` (sur `main` depuis
  #68) pour vérifier qu'aucune ligne ne re-rend le transcript et que la
  reconnexion n'ajoute pas de frames.

## 5. Risques et décisions

| Risque | Mitigation |
| --- | --- |
| L'auto-compaction Claude reste silencieuse au début | Afficher `status:compacting` quand il existe ; sinon la frontière à l'arrivée. Documenter. |
| Les RFD ACP changent ou ne sont pas implémentées | Traitées comme brouillons ; flags off par défaut ; capacité client ≠ émission agent ; surface effective mesurée avant merge (Gate B). |
| Sous-agents : contenu non synchronisé | Projection host-local du journal ; viewer distant voit statut/durée/compteur terminal + « transcript non disponible », jamais un faux vide. |
| Bloat du doc / trafic de sync | Métadonnées seules par défaut ; zéro écriture par outil enfant ; cap 8 Ko seulement en opt-in. |
| Perf UI non mesurable | Compteurs par vue et par ligne (Task 8 Step 0) ; `perf.rs` seul ne suffit pas. |
| Ordre inter-tour | Entrées `System`, ordre du doc, pas de tri par timestamp. |
| Horloges désynchronisées entre appareils | Aucune décision par timestamp : tie-break par ordre du doc (§1.5). |
| Secrets dans un résumé ou une réponse libre | Résumé journal-only par défaut ; réponse `secret` jamais loggée/exportée ; opt-in doc avec avertissement. Export testé. |
| Cursor SDK beta | Le shim ignore l'inconnu ; `BoundaryOnly` si `summary-started` absent. |
| Antigravity protocole fermé | Sonde + déclaration `None` si aucun signal ; fix du canal de réponse ou retrait. |
| Journal survivant à la suppression du chat | `RunJournal::discard` branché sur `DeleteChat` + test de non-résurrection ; rétention documentée. |
| Événements éphémères dans le journal | Chemin broadcast-only, test `not_journaled` ; `publish` ne journalise que les faits durables. |
| Double écriture des réponses | Writer unique + idempotence (`request_id`, appareil, op) ; test de non-doublon. |
| Durée sous-agent non reconstructible | `None` après crash (journal sans horodatage) ; ne pas inventer. |
| Flag ACP global annoncé à tous | Allowlist par `HarnessId`, validée agent par agent. |
| Deux compactions dans un run | Id par compaction, test dédié. |
| Export oublie les nouveaux parts | `doc/src/export.rs` au périmètre, test golden. |
| Vieux client qui réécrit le doc | `kind: compaction` dégradé en `Text` vide : documenté, pas de réécriture silencieuse. |
| Reconnexion en boucle ou écran blanc | Backoff borné + jitter, plafond puis `Failed { retryable }` ; dernière frame et échos conservés ; test de reprise. |
| Spinner sans état nommé | Phases `starting`/`waiting_model`/`streaming`/`tool` ; timeout explicite → `Degraded`, jamais une animation infinie. |
| Message perdu pendant une coupure | File locale avec raison nommée, accusé du doc, dédoublonnage par id ; test de non-perte et non-doublon. |
| Raccourci qui approuve par accident | Échap ne résout jamais une permission ; approbation = action explicite de la carte ; test de précédence. |
| Animation ad hoc coûteuse | Réutilisation de `motion.rs`/`thinking_orbs.rs` et du pulse clock ; compteurs Task 8, aucune boucle par frame. |
| Plan/tâches ignoré ou transitoire | Dernier `ToolCall::Todo` dérivé en pur, carte persistante au-dessus du composer tant qu'il reste du travail ; liste vide = plan effacé ; test doc + UI. |

**Décisions retenues**

1. Sous-agents : **projection du journal de run**, pas de doc enfant, pas de
   TTL, pas de sync. Le journal suit la vie du chat.
2. Résumé de compaction : **journal-only par défaut** (fetch
   `GetCompactionSummary`) ; opt-in `SummaryInDoc` ≤ 8 Ko ; l'export suit la
   même politique et n'exporte jamais une réponse `secret`.
3. ACP : `elicitation.form` et `session.compaction` restent derrière des flags
   **off par défaut** ; activation seulement après vérification sur un agent
   réel et mise à jour de la matrice (Gate B).
4. Sorties d'outils complètes **journal-only** (A2), transparence par fetch et
   progrès live.
5. `WatchActivity` éphémère pour la progression, comme `WatchReasoning`.
6. Réponses concurrentes : gagnant = premier élément dans l'ordre du doc Loro ;
   aucun tri par horloge murale.
7. Fluidité : Task 8 s'appuie sur `perf.rs`, déjà sur `main` (#68 mergée) ;
   Step 0 étend les compteurs au lieu de dupliquer l'instrumentation.
8. Connexion : état UI enrichi (`Reconnecting`, `Degraded`) et backoff côté
   viewport ; `RunPhase` reste éphémère, jamais un nouvel état durable du wire.
9. Interaction : parité clavier sur les actions fréquentes, file visible et
   annulable ; Échap ferme/arme le stop mais ne résout jamais une permission.
10. Exhaustivité : le plan courant est une surface de premier rang (carte
    persistante), pas un détail de chip ; la progression et la phase restent
    éphémères et déclarées telles quelles.

## 6. Séquencement et PRs

| PR | Contenu | Dépend de | Estimation |
| --- | --- | --- | --- |
| 1 | Task 0 — surface + capacités ACP | — | 0,5–1 j |
| 2 | Task 1 — réponses/décisions visibles | 1 | 1–2 j |
| 3 | Task 2 + 3.1 + 3.2 + 3.3 + 3.6 — socle compaction + Claude/Codex/Cursor/Muse | 1 ; rebasée sur 2 (`parts.rs`, `schema.rs`, `doc_host.rs`, `sessions.rs`) | 3–4 j |
| 4 | Task 3.4 + 3.5 + 3.7 — famille ACP (chemin partagé), Antigravity (sonde), Mock | 1 (flags), 3 (proto/fold compaction) | 2–3 j |
| 5 | Task 4 — projection journal sous-agent + RPC | 1, 2, 3 (mêmes fichiers chauds) | 2–3 j |
| 6 | Task 5 — UI sous-agent | 5 | 2–3 j |
| 7 | Task 6 — questions | 1, 2 | 2–3 j |
| 8 | Task 7 — plan persistant + progrès | 1, 5 (`WatchActivity`) | 2–3 j |
| 9 | Task 9 — durabilité (tests croisés) | 2, 3, 5 | 2 j |
| 10 | Task 8 — fluidité (compteurs par vue/ligne étendus) | 1 ; rebasée sur 6/7/8 (points d'instrumentation) | 2–3 j |
| 11 | Task 10 — connexion fluide (état, backoff, `RunPhase`) | 5 (`WatchActivity`), 6/7/8 (mêmes surfaces UI) ; après 10 (compteurs) | 2–3 j |
| 12 | Task 11 — interaction directe (clavier, file, actions inline) | 1, 2, 6, 7, 8 ; après 11 | 2 j |
| 13 | Task 12 — observabilité/docs | toutes | 1 j |

Total : ~24–33 jours de travail effectif, ~28–40 jours calendaires à une
personne. Les tâches ne sont pas indépendantes : `doc/src/parts.rs`,
`doc/src/schema.rs`, `engine/src/sessions.rs`, `engine/src/doc_host.rs`,
`engine/src/run_journal.rs`, `rpc/src/lib.rs`, `proto/src/view.rs` et les
surfaces UI (`state.rs`, `shell.rs`, `composer.rs`, `transcript.rs`) sont
touchés par plusieurs PRs, et une PR qui les modifie en parallèle d'une autre
produit des conflits de fold ou de focus, pas seulement des conflits textuels.
Séquencement utile :

1. **PR 1 seule.** Elle fixe les surfaces et les flags ; une matrice modifiée
   en parallèle crée des affordances fantômes.
2. **PR 2 puis PR 3, sérialisées.** PR 2 (réponses append-only + writer unique)
   est plus petite et fixe le contrat d'écriture ; PR 3 rebase et ajoute
   `Compaction` sur les mêmes fichiers.
3. **PR 4 puis PR 5.** PR 4 ne dépend que du proto de PR 3 ; PR 5 retouche
   `sessions.rs`/`doc_host.rs` et branche `discard` sur `DeleteChat`.
4. **PRs 6, 7, 8.** PR 6 et PR 8 après PR 5 ; PR 7 après PR 2. Toutes trois
   touchent `ui/src/transcript.rs` et `ui/src/state.rs` : les enchaîner plutôt
   que les paralléliser. PR 9 vient après 2/3/5 : ce sont les tests de
   durabilité de ces trois contrats, pas une PR indépendante.
5. **PR 10 largement parallèle** — `perf.rs` et `scripts/frame-budget.sh` sont
   isolés — mais ses points d'instrumentation vivent dans `shell.rs`,
   `transcript.rs` et `composer.rs`. La lancer tôt pour disposer des compteurs,
   la rebaser sur les PRs UI, puis re-mesurer avant PR 13.
6. **PR 11 puis PR 12.** Les deux touchent les mêmes surfaces UI que les
   PRs 6/7/8 ; les enchaîner après elles évite des conflits de focus et de
   rendu. PR 11 dépend de `WatchActivity` (PR 5) pour `RunPhase` ; PR 12
   dépend de l'ordre de file fixé en PR 2. PR 13 (docs) attend tout le reste.

## 7. Definition of Done par agent

| Agent | Compaction visible | Sous-agent visible | Questions répondues + persistées | Plan | Progrès | No-op silencieux |
| --- | --- | --- | --- | --- | --- | --- |
| Claude Code | oui (`status` + frontière + résumé) | oui | oui (natif) | oui | déclaré `None` | supprimé |
| Codex | oui (frontière) | oui | non (approvals seulement, déclaré) | oui | oui (streaming) | supprimé |
| Cursor | oui (frontière) | oui | non (déclaré) | oui | partiel si émis, sinon final | supprimé |
| Grok | oui si Gate B validée et update observé | oui (tail, hôte) | oui (elicitation) ou fallback déclaré | oui | partiel si émis, sinon final | supprimé |
| OpenCode | oui si Gate B validée et update observé | oui (hôte) | oui/fallback | oui | partiel si émis, sinon final | supprimé |
| Devin | oui si Gate B validée et update observé | selon émission, déclaré | oui/fallback | oui | partiel si émis, sinon final | supprimé |
| Copilot | idem Devin | idem | idem | oui | partiel si émis, sinon final | supprimé |
| Hermes | idem | idem | idem | oui | partiel si émis, sinon final | supprimé |
| Pi | idem | idem | idem | oui | partiel si émis, sinon final | supprimé |
| Cline | idem | idem | idem | oui | partiel si émis, sinon final | supprimé |
| Qoder | idem | idem | idem | oui | partiel si émis, sinon final | supprimé |
| Antigravity | déclaré `None` si aucun signal | déclaré `None` | réparé ou déclaré `None` | oui | partiel si émis, sinon final | supprimé |
| Muse | oui (item démasqué) | oui | oui (natif) | oui | oui | supprimé |
| Mock | oui | oui | oui | oui | oui | — |

Le tableau couvre les **14 harnesses** (dont 8 agents ACP : Grok, OpenCode,
Devin, Copilot, Hermes, Pi, Cline, Qoder). Chaque ligne « déclaré » doit être
couverte par `interaction_surfaces_are_declared` et par une ligne visible dans
Settings → Agents. Gate B n'est jamais un flag global : l'allowlist est indexée
par `HarnessId`, et un agent supplémentaire ne peut passer à « oui » qu'après
une exécution réelle qui montre le signal.

Lecture des colonnes : **Compaction visible** = métadonnées dans le doc sur
tous les appareils + résumé fetchable sur l'hôte (sauf `SummaryInDoc`) ;
**Sous-agent visible** = transcript sur l'hôte, statut/durée/compteur terminal
partout. Aucune ligne ACP ne peut passer « oui » avant la validation réelle de
Gate B ; d'ici là elle reste `None` et l'affordance est masquée. L'auto-allow
des approbations (I6, Task 12) n'est **pas** une ligne de feed : quand un niveau
délègue (Full access, ou l'auto-refus de Read only), le niveau est la trace et
le CLI n'a jamais posé de demande à afficher ; hors délégation, une décision de
Komet doit être visible. Un agent qui n'applique rien le déclare, il ne reste
pas silencieux.

### 7.1 Definition of Done UX

| Surface | État visible | Action immédiate | Clavier | Non-blocage |
| --- | --- | --- | --- | --- |
| Démarrage moteur | `Connecting`, puis `Ready` | Retry si échec | Oui | La dernière frame reste |
| Connexion perdue | `Reconnecting` + tentative + compte à rebours | Retry now | Oui | Transcript et échos conservés ; envoi mis en file |
| Démarrage du run | `starting` (harness) | Stop | Oui | Le compositeur reste actif |
| Modèle en attente | `waiting_model` | Stop, steer | Oui | Aucun spinner sans nom |
| Run en cours | `streaming`/`tool` + activité | Steer, stop | Oui | Question/permission dans une file |
| Question | Carte + progression | Répondre, passer | Oui | Frappe et steer possibles |
| Permission | Carte + timeout | Approuver, refuser | Oui, explicite | Jamais résolue par Échap |
| Échec de tour | Erreur + `retryable` | Relancer, détails | Oui | Le chat reste lisible |
| Plan / tâches | Carte persistante `done/total` + lignes | Ouvrir le plan complet | Oui | Ne bloque ni frappe ni scroll |
| Sous-agent | Statut + compteur terminal | Ouvrir l'onglet | Oui | Le parent continue |
| File de prompts | « N queued » + liste | Annuler un item | Oui | Drain automatique |

Une ligne sans état, action ou clavier est un manque UX, pas un détail de
finition : elle bloque le critère de la tâche correspondante.

## 8. Recommandations de revue (v3–v4)

Ces points complètent les tâches : ce sont les vérifications à opposer à une PR
qui « marche » mais dérive du contrat.

### 8.1 Avant d'écrire du code

- **Figer la matrice avant les adaptateurs.** PR 1 seule, relue avec
  `interaction_surfaces_are_declared` vert sur les 14 harnesses. Une PR qui
  ajoute une affordance sans ligne de matrice est refusée.
- **Tester le modèle avant le wire.** Pour chaque variante : fold,
  aller-retour doc, et `not_journaled` si éphémère. Les adaptateurs ne doivent
  pas être le lieu où l'on découvre qu'un événement ne se replie pas.
- **Brancher le journal sur la vie du chat dès Task 4.** `discard` sur
  `DeleteChat`, non-résurrection testée, rétention documentée ; sans cela,
  « host-local » veut dire « éternel ».

### 8.2 Contrats à ne pas laisser dériver

- **Un seul writer par fait durable.** `respond_input` écrit la partie et émet
  `InputResolved { answers }` ; le fold ne fait que tamponner. Tests
  d'idempotence (même `request_id`) et « une réponse distante n'est jamais
  rejouée ».
- **Gagnant du merge ≠ réponse remise.** `applied` est écrit par le writer
  local ; l'UI montre les deux états. Ne jamais déduire `applied` de l'index
  dans `responses`.
- **Aucune valeur inventée.** `tokens_after` reste `None` jusqu'au prochain
  `ContextWindow` natif ; durée de sous-agent `None` après crash ; sans
  frontière de compaction, la surface est `None`. Un fallback qui devine est
  un bug, pas une robustesse.
- **Clore les états `InProgress` au restart.** Comme `mark_abandoned` pour un
  sous-agent, une compaction restée `InProgress` après un crash doit être
  fermée en `Failed` (ou reprise explicitement), sinon la ligne reste bloquée
  pour toujours sur tous les appareils.
- **La sécurité est dans le writer, pas seulement dans l'UI.** Résumé
  journal-only par défaut ; champ `secret` masqué, absent du log, de l'export
  et du broadcast ; cap 8 Ko en UTF-8 sûr. `export.rs` n'exporte aujourd'hui
  ni Input, ni Permission, ni Compaction : sa revue est obligatoire.
- **Compatibilité additive assumée.** Vieux wire sans `answers`/`source` :
  `resolved` seul fait foi. Vieux client qui réécrit `kind: compaction` en
  `Text` vide : documenté, sans réécriture silencieuse. Tests de round-trip
  dans les deux sens.

### 8.3 UI et perf

- **Mesurer avant d'ajouter des lignes.** PR 10 tôt : compteurs
  `ContextUsage`/rail/par ligne, `rail = 0` pendant un stream,
  `scripts/frame-budget.sh` étendu. Aucune nouvelle boucle par frame ; les
  nouvelles lignes passent par `Entity::cached`.
- **File passive, action explicite.** Une question ne bloque ni la frappe ni un
  steer ; Escape ne résout pas une demande en conflit ; l'UI distingue une
  demande `actionable` d'une demande historique.
- **La connexion ne vide jamais l'écran.** Un `Reconnecting` conserve la
  dernière frame et les échos ; les watches sont recréés, pas les données
  effacées. Un `Failed` garde le bouton Retry et la raison exacte.
- **Pas de spinner anonyme.** Chaque attente a une phase nommée, un timeout et
  une sortie. `starting` qui dure devient `Degraded`, pas un orbe éternel.
- **Le clavier ne doit pas pouvoir approuver par accident.** Échap ferme ou
  arme le stop ; une résolution de permission exige une action explicite de
  la carte. Tester l'ordre des priorités, pas seulement le raccourci heureux.
- **La file est une surface, pas un compteur.** « N queued » doit s'ouvrir,
  montrer les items et permettre l'annulation ; sinon l'utilisateur ne sait
  pas ce qui va partir.

### 8.4 Critères de refus immédiats

Une PR est refusée si elle : ajoute un rendu par frame ; journalise un fait
éphémère ; annonce une capacité ACP sans exécution réelle ; écrit deux fois le
même fait durable ; logge, exporte ou diffuse un champ `secret` ; affiche une
durée ou un `tokens_after` inventés ; modifie `parts.rs`/`sessions.rs` sans
test de fold correspondant ; introduit un spinner sans état nommé ; laisse une
action fréquente souris-only ; ou vide le transcript pendant une reconnexion.

### 8.5 Points à trancher avant merge

1. **Rétention du journal** : durée et taille max par chat, et comportement à
   la limite. Recommandation : borne par chat + `Unavailable` explicite plutôt
   qu'une troncature silencieuse.
2. **`GetCompactionSummary`** : RPC hôte uniquement ? Recommandation : oui,
   comme `WatchSubagent` ; un viewer distant voit les métadonnées et l'état
   « résumé non disponible sur cet appareil ».
3. **Entrées `System`** : vérifier que `rail.rs`, `stats.rs` et `export.rs`
   n'affichent pas une entrée vide et ne présentent pas les métadonnées de
   compaction comme du texte utilisateur.
4. **Compaction `InProgress` non terminale** : définir si elle est créée dans
   le doc dès l'ouverture (visible partout, mais bloquée si l'hôte meurt) ou
   seulement à l'état terminal (plus sûr, mais silence pendant l'opération).
   Recommandation : état éphémère `WatchActivity` pendant, partie doc à
   `Completed`/`Failed`, plus la clôture au restart ci-dessus.
5. **Plafond de reconnexion** : nombre de tentatives automatiques avant
   `Failed { retryable }` (proposition : 6) et politique de jitter.
   Recommandation : borné et visible ; jamais une boucle silencieuse.
6. **Jeu de raccourcis** : figer une seule table (`mod-shift-c`,
   `mod-shift-q`, `mod-.`, Entrée/Échap) et la tester comme un contrat. Un
   raccourci ajouté sans test de précédence est refusé.
7. **File de prompts** : l'annulation suffit-elle, ou faut-il réordonner ?
   Recommandation : annulation seule dans cette passe ; le réordonnancement
   est un besoin séparé, pas un prérequis.
