# Transparence agent — compaction, sous-agents, questions

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development
> (recommended) or superpowers:executing-plans to implement this plan task-by-task.
> Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Rendre visible dans l'UI tout ce que le CLI de base montre — actions,
sorties, compaction, sous-agents, questions et décisions — au moment où cela se
produit, de façon persistante, non bloquante et honnête sur les capacités de
chaque agent.

**Architecture:** Un `AgentEvent` canonique par fait observable, normalisé dans
l'adaptateur du harness ; l'engine route et persiste (doc parent avec
métadonnées bornées, entrées `System`, projection à la demande du journal de
run pour les sous-agents, canal éphémère `WatchActivity`) ; l'UI rend une seule
famille de lignes et une seule file de demandes. La matrice d'interaction
déclarée par harness empêche toute affordance impossible.

**Tech Stack:** Rust edition 2024, serde additif, Loro (doc CRDT), gpui
(jomvick/zed), ACP v1, Claude stream-json, Codex app-server, Cursor SDK shim,
Muse MSP, Antigravity NDJSON.

**Spec:** Demande utilisateur 2026-10-27 — « transparence totale, aucune limite
entre le CLI de base et le harness Komet, fluidité, capture des questions ».
Complète `2026-08-26-permission-ui.md` (ne le remplace pas) et s'appuie sur
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
réglage ; les 7 agents ACP fusionnés en un chemin partagé ; sous-agents servis
par le journal de run au lieu d'un doc enfant ; compteur d'outils enfant écrit
seulement à l'état terminal ; I2 réduit à ce qui est réellement durable ;
tie-break des réponses défini par l'ordre du doc ; résumé de compaction
journal-only par défaut avec export/sync explicites.

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

## 0. Invariants de transparence

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
- **I6 — Sécurité.** Deny gagne, aucun allow silencieux, tout auto-allow est
  tracé, un timeout est visible.

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
    /// (ACP `compactionId`), sinon synthétisé par l'adaptateur.
    #[serde(rename_all = "camelCase")]
    ContextCompaction {
        id: String,
        status: CompactionStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        trigger: Option<String>,       // "auto" | "manual" | valeur agent
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tokens_before: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tokens_after: Option<u64>,
        /// Remplacement complet du résumé accumulé.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
        /// Ajout au résumé accumulé (ACP `compaction_summary_chunk`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary_delta: Option<String>,
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
    /// Rempli seulement quand la politique vaut `SummaryInDoc` : borné à
    /// COMPACTION_SUMMARY_MAX_BYTES, jamais une valeur par défaut.
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
    responses: Vec<InputResponse>,
    source: komet_proto::QuestionSource,
    /// Appareil qui a répondu (horodatage porté par l'entrée).
    decided_by: Option<String>,
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
(`Done`/`Failed`) ou à la récupération au restart. Le décompte live (« 3
outils ») vient de `WatchActivity`, jamais du doc synchronisé.

Règles de fold (`fold_event_into_parts`) :

- `ContextCompaction` : cherche `compaction_id` ; met à jour en place. En mode
  `SummaryInDoc`, `summary` remplace et `summary_delta` ajoute ; en mode
  `JournalOnly` (défaut), le texte n'entre pas dans le doc, seul
  `summary_bytes` et `summary_in_journal = true` sont posés. Crée la partie si
  absente, y compris pour un `Completed` inter-tour.
- Une réponse à une question s'accumule dans `responses` (append-only) ; voir
  le tie-break §1.5.
- `ToolProgress` : **aucun fold** (éphémère).
- `InputResolved` : tamponne `answers` sur la partie dont `request_id` correspond.
- `PermissionResolved` : inchangé, plus `decided_by` fourni par le writer.
- `Subagent` : inchangé (statut + compteur d'outils + durée), jamais le contenu.

### 1.3 Persisté vs éphémère

| Fait | Persisté dans le doc | Canal éphémère |
| --- | --- | --- |
| Compaction (début, fin, échec) | oui, `MessagePart::Compaction` | `WatchActivity` pour l'état `inProgress` |
| Résumé de compaction | métadonnées seulement (`summary_bytes`, `summary_in_journal`) ; texte si opt-in `SummaryInDoc` | journal de run, fetch `GetCompactionSummary` |
| Progression d'outil | non | `WatchActivity` (`toolProgress`) |
| Sous-agent : statut/durée/nb d'outils | terminal uniquement, sur la chip parente | `WatchActivity` (statut + décompte + preview une ligne) |
| Sous-agent : transcript | non — projection du journal de run, host-local | `WatchSubagent` |
| Question et réponse | oui, partie `Input` | — |
| Permission et décision | oui, partie `Permission` | — |
| Plan | oui, `ToolCall::Todo` sur `LIVE_PLAN_TOOL_ID` | — |

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
| Cursor | `None` (pas de canal de réponse SDK) | `BoundaryOnly` (`summary-started/completed`) | `Tagged` (`taskUpdate`) | `ToolChip` | `Final` |
| Grok (ACP) | `Elicitation` + fallback `PermissionShaped` | `Lifecycle` (RFD draft, gated) | `NestedSession` + tail `chat_history.jsonl` | `ToolChip` | `Final` |
| OpenCode (ACP) | `Elicitation` + fallback | `Lifecycle` (RFD draft, gated) | `NestedSession` | `ToolChip` | `Final` |
| Devin (ACP) | `Elicitation` + fallback | `Lifecycle` (RFD draft, gated) | `TaggedIfEmitted` | `ToolChip` | `Final` |
| GitHub Copilot (ACP) | `Elicitation` + fallback | `Lifecycle` (RFD draft, gated) | `TaggedIfEmitted` | `ToolChip` | `Final` |
| Hermes (ACP) | `Elicitation` + fallback | `Lifecycle` (RFD draft, gated) | `TaggedIfEmitted` | `ToolChip` | `Final` |
| Pi (ACP) | `Elicitation` + fallback | `Lifecycle` (RFD draft, gated) | `TaggedIfEmitted` | `ToolChip` | `Final` |
| Cline (ACP) | `Elicitation` + fallback | `Lifecycle` (RFD draft, gated) | `TaggedIfEmitted` | `ToolChip` | `Final` |
| Antigravity | `Native` (`ask_question`, canal à réparer) | `None` (à sonder) | `None` | `ToolChip` | `Final` |
| Muse | `Native` (`request_user_input`) | `Lifecycle` (item démasqué) | `Tagged` (`subagent_spawn`) | `LivePanel` | `Streaming` (révisions d'item) |
| Mock | `Native` | `LifecycleWithSummary` | `Tagged` récursif | `LivePanel` | `Streaming` |

Le test `interaction_surfaces_are_declared` doit échouer si un `HarnessId`
n'a pas de déclaration explicite.

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
- Modify: chaque `crates/harness/src/{claude,codex,cursor,antigravity,muse,mock}/mod.rs`.
- Modify: `crates/harness/src/acp/mod.rs` (`initialize`).
- Modify: configuration des harness (flags `KOMET_ACP_ELICITATION=off|form`,
  `KOMET_ACP_COMPACTION=off|on`, défaut `off`).
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

- [ ] **Step 1:** Écrire `interaction_surfaces_are_declared` : itérer sur les 13
  `HarnessId` et exiger une déclaration non-`none` ou explicitement `None` par
  famille (un match exhaustif, pas un `_ =>`).
- [ ] **Step 2:** Écrire `acp_capabilities_are_unchanged_when_experiments_off` :
  sans `KOMET_ACP_ELICITATION=form` ni `KOMET_ACP_COMPACTION=1`, le JSON
  `clientCapabilities` est **identique à aujourd'hui** (golden). Puis
  `acp_capabilities_advertise_when_enabled` : avec les deux variables, le JSON
  contient `elicitation.form` et `session.compaction`, sans régresser `fs` /
  `terminal`.
- [ ] **Step 2b:** Valider l'annonce sur un agent réel avant de changer le
  défaut (Grok ou OpenCode, version épinglée) : consigner dans
  `docs/research/acp.md` (version, capacités lues, update reçu ou absence).
  Tant que la validation n'est pas faite, le défaut reste `off` et la matrice
  décrit la surface effective, pas la surface espérée.
- [ ] **Step 3:** Implémenter les types et les overrides par harness selon la
  table §2. Pour les ACP, une seule implémentation partagée, paramétrée par
  `HarnessId` pour les cas `TaggedIfEmitted`. Tant que Gate B est fermée, la
  déclaration ACP de compaction/questions reste `None`/`PermissionShaped`.
- [ ] **Step 4:** Ajouter `surface` à `HarnessDescriptor` et l'afficher dans
  Settings → Agents sous forme de lignes courtes (« Compaction : oui »,
  « Questions : non supportées »).
- [ ] **Step 5:** Exécuter `cargo test -p komet-harness --test surfaces`,
  `cargo test -p komet-engine registry`, `cargo check -p komet-ui`.

**Critère :** aucune affordance UI ne peut être affichée sans une capacité
déclarée ; par défaut les agents ACP ne voient **aucune** annonce
expérimentale ; une fois Gate B validée, ils reçoivent les deux capacités.

---

### Task 1 — Réponses et décisions visibles dans le fil

**But :** arrêter de perdre les réponses et les décisions.

**Files:**
- Modify: `crates/proto/src/agent.rs` (`InputResolved.answers`,
  `PermissionRequested.tool_call_id`).
- Modify: `crates/doc/src/parts.rs` (`Input.responses`, `source`, `decided_by` ;
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
- [ ] **Step 1:** Test `old_input_part_without_responses_still_reads` : un doc
  contenant `{"kind":"input","id":"in-r1","requestId":"r1","questions":[],"resolved":true}`
  se relit avec `responses: []`, `source: Native`.
- [ ] **Step 2:** Test `input_resolved_appends_a_response` : `resolve_input("r1",
  answers)` ajoute les libellés et le texte libre à `responses`, et un replay
  les restitue.
- [ ] **Step 3:** Test `concurrent_answers_converge_on_doc_order` : deux
  réplicas ajoutent une réponse hors ligne, fusionnent, et élisent le même
  gagnant (premier élément dans l'ordre du doc, §1.5) ; le perdant est affiché
  comme conflit (dérivé de l'index) et n'est jamais transmis à l'agent.
- [ ] **Step 4:** Implémenter le fold et le writer ; brancher `respond_input`
  pour transmettre les réponses à l'événement.
- [ ] **Step 5:** Remplacer `pending_input_request` et
  `pending_permission_request` par des fonctions retournant **toutes** les
  demandes ouvertes, avec `entry_id` et ordre d'arrivée.
- [ ] **Step 6:** Rendre `PermissionRow` et `QuestionRow` : icône, résumé,
  décision/réponse, device, horodatage, lien vers le tool call. Remplacer le
  chip passif `InputChip` (`transcript.rs`, fonction `input_chip`).
- [ ] **Step 7:** `cargo test -p komet-doc && cargo test -p komet-ui`.

**Critère :** l'historique affiche « Question : … → réponse : … » et
« Permission : git push → Allow (device X, 14:32) », y compris après restart.

---

### Task 2 — Compaction : socle proto/doc/engine/UI

**Files:**
- Modify: `crates/proto/src/agent.rs` (`CompactionStatus`,
  `AgentEvent::ContextCompaction`).
- Modify: `crates/doc/src/constants.rs` (`COMPACTION_SUMMARY_MAX_BYTES = 8 * 1024`,
  `CompactionSummaryPolicy`).
- Modify: `crates/doc/src/parts.rs` (`MessagePart::Compaction`, fold).
- Modify: `crates/doc/src/schema.rs` (`DocPartJson` additif).
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
- [ ] **Step 1:** Test `compaction_part_round_trips_and_old_doc_ignores_it`.
- [ ] **Step 2:** Test `compaction_fold_updates_in_place_by_id` : `inProgress`
  puis `completed` avec `tokens_before`, `trigger` et `summary` ; un
  `summary_delta` successif s'ajoute ; un `summary` concret remplace.
- [ ] **Step 3:** Test `inter_turn_compaction_creates_a_system_entry` : hors
  stream, une entrée `System` complète est insérée à la bonne position.
- [ ] **Step 4:** Tests de politique :
  `journal_only_summary_never_reaches_the_doc` (défaut : `summary: None`,
  `summary_bytes` et `summary_in_journal` posés) ;
  `summary_in_doc_is_capped` (opt-in : 100 Ko tronqués à 8 Ko,
  `summary_bytes` conserve la taille d'origine).
- [ ] **Step 4b:** Test `get_compaction_summary_reads_the_journal` : fetch sur
  l'hôte → texte ; sur un device sans journal → `unavailable`, jamais une
  chaîne vide silencieuse.
- [ ] **Step 4c:** Test `export_honours_summary_and_secret_policies` : en mode
  journal-only l'export contient la ligne mais pas le texte ; une réponse
  `secret` n'est jamais exportée.
- [ ] **Step 5:** Implémenter l'insertion (`fold into streaming assistant` sinon
  `insert_system_entry`), la politique de résumé, la publication
  `WatchActivity`, le fetch journal et l'abandon (`mark_abandoned_streams`
  ferme un `inProgress` en `Failed` avec raison).
- [ ] **Step 6:** UI : ligne compacte « Context compacted · 165k → 12k · auto ·
  14:32 » avec état `in_progress` (pulse discret), résumé dépliable via
  `GetCompactionSummary` (état « disponible sur l'hôte » si le journal est
  absent), et un `RowKind::Compaction` qui ne participe pas au regroupement
  d'outils.
- [ ] **Step 7:** Vérifier que le popover « Context » et la timeline racontent la
  même chose (compteur et dernière raison), sans double rendu.
- [ ] **Step 8:** `cargo test -p komet-doc && cargo test -p komet-engine &&
  cargo test -p komet-ui`.

**Critère :** une compaction est visible au bon endroit dans le fil, dans l'ordre
d'arrivée, et survit au restart.

---

### Task 3 — Compaction : un adaptateur par famille de signal

Chaque sous-tâche est indépendante et peut être une PR. Le contrat commun :
produire `AgentEvent::ContextCompaction` avec le meilleur signal disponible ;
si aucun signal, ne rien produire et laisser la matrice déclarer
`CompactionSurface::None`. Les sept agents ACP partagent un seul adaptateur
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
    status: InProgress, trigger: None }` avec un id synthétique stable par run.
  - `subtype == "compact_boundary"` → `ContextCompaction { status: Completed,
    trigger, tokens_before }`.
  - message `is_compact_summary` → `ContextCompaction { summary }` sur le même
    id (ou `summary_delta` si le CLI le découpe), et **pas** une entrée
    utilisateur parasite.
- Tests : `compact_boundary_camel_case`, `compact_boundary_snake_case`,
  `status_compacting_emits_in_progress`, `compact_summary_is_not_a_user_turn`,
  `old_wire_without_compact_fields_still_parses`.

**Piège :** ne pas confondre un `system/status` d'un autre type avec une
compaction ; ne pas émettre `Completed` sans frontière.

#### 3.2 Codex — `crates/harness/src/codex/`

**Signal :** notification `thread/compacted` (après coup, sans résumé).

**Changement :**
- `normalize.rs` : `compacted_event()` émet **les deux** : le
  `ContextWindow` actuel (jauge) et un `ContextCompaction { status: Completed,
  trigger: None }`.
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

#### 3.4 Famille ACP — chemin partagé (Grok, OpenCode, Devin, Copilot, Hermes, Pi, Cline)

**Signal :** `session/update` `compaction_update` et
`compaction_summary_chunk` (RFD ACP Session Compaction, **brouillon**), si
l'adaptateur les implémente et si le client a annoncé
`clientCapabilities.session.compaction` (Gate B). Ces sept agents partagent le
même normalizer : une implémentation, une fixture, pas sept variantes.

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
  `docs/research/acp.md` (binaire, version épinglée, update observé ou absent).
  Tant qu'un agent n'est pas vérifié, sa surface reste `None` — jamais une
  promesse non testée. OpenCode n'a plus de `session.compacted` public ;
  l'ACP est la seule voie. `pi-acp` est communautaire et épinglé : vérifier sa
  version avant `LifecycleWithSummary`, sinon `Lifecycle`.

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
- Ce scénario alimente les tests engine/UI et `scripts/dev-demo.sh` (option
  `--scenario compaction` ou variable d'environnement mock).
- Test : `mock_compaction_scenario_is_deterministic`.

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
- Modify: `crates/rpc/src/lib.rs` (`WATCH_SUBAGENT`).
- Modify: `crates/engine/src/rpc.rs` (handler).
- Test: `crates/engine/tests/subagents.rs` (nouveau).

**Design :**
- Clé : `(chat_id, parent_tool_use_id)`. Pas d'id composé, pas de doc, pas de
  TTL : le journal suit déjà la vie du chat (`discard`, suppression, import).
- `WATCH_SUBAGENT {chatId, parentToolUseId}` : rejoue le journal filtré puis
  s'abonne au hub d'événements du chat (déjà diffusé avec `seq`) pour le live.
  Un événement imbriqué plus profond reste sous son parent direct.
- Host-local par construction : le journal vit sur l'hôte. Un viewer distant
  reçoit `unavailable` (même contrat que `GetToolDetail`), jamais un faux
  transcript vide.
- Doc parent : la chip reçoit `subagent_status` (existant), puis **une seule
  écriture** terminale de `subagent_duration_ms` et `subagent_tool_count`. Le
  décompte live et la preview passent par `WatchActivity`.
- Restart : `mark_abandoned` ferme un `Running` en `Failed` ; le transcript
  reste reconstructible depuis le journal. Journal absent/rollé → l'UI affiche
  « transcript non disponible sur cet appareil ».
- Les commentaires de `proto/src/agent.rs:1793` et `doc/src/parts.rs:462` qui
  annoncent « le doc du sous-agent » sont corrigés en « projection du
  journal ».

**Steps:**
- [ ] **Step 1:** Test `subagent_events_are_journaled_and_filtered` : un run
  avec texte/outils imbriqués ne modifie aucune partie texte du doc parent ;
  la projection retourne ces événements dans l'ordre du journal.
- [ ] **Step 2:** Test `nested_subagents_stay_under_their_direct_parent` :
  `A → B` n'apparaît pas dans le transcript de `A` au premier niveau.
- [ ] **Step 3:** Test `parent_chip_is_written_once_at_terminal` : zéro
  écriture doc pendant le run, une écriture à `Done`/`Failed`, y compris après
  restart.
- [ ] **Step 4:** Test `watch_subagent_replays_then_streams` : les événements
  déjà journalisés arrivent avant les nouveaux, sans doublon (`seq`).
- [ ] **Step 5:** Test `subagent_transcript_without_journal_is_explicit` :
  `unavailable`, pas de transcript vide ni d'erreur générique.
- [ ] **Step 6:** Implémenter la projection et le handler ; vérifier qu'aucun
  contenu enfant n'est écrit deux fois (journal = seule copie).
- [ ] **Step 7:** `cargo test -p komet-engine subagent`.

**Critère :** le transcript imbriqué est complet, live, récursif et
reconstructible après restart, sans doc enfant ni écriture de contenu dans le
doc parent ; le doc parent ne bouge qu'à l'état terminal.

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

**Steps:**
- [ ] **Step 1:** Cliquer une chip de spawn ouvre l'onglet `Subagent` ; le
  panneau affiche le transcript live (replay + flux) et un en-tête
  modèle/statut/durée.
- [ ] **Step 2:** Une chip imbriquée ouvre un second onglet (récursion), sans
  mélanger les niveaux.
- [ ] **Step 3:** La chip parente affiche « Running · 3 outils · 12 s » via
  `WatchActivity` ; à la fin, « Done · 3 outils · 41 s » depuis la valeur
  terminale du doc.
- [ ] **Step 4:** `WatchActivity` fournit une preview d'une ligne (dernier outil
  ou dernier texte), sans écriture dans le doc parent.
- [ ] **Step 5:** Sur un viewer distant (journal absent), l'onglet affiche le
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
| Devin / Copilot / Hermes / Pi / Cline | dépend de l'adaptateur | test de surface : `None` si aucun événement |
| Muse | item `subagent_spawn` | fixture `muse_subagent` |
| Antigravity | aucun | déclaration `None`, pas d'affordance |
| Mock | taggé récursif | scénario `subagent_nested` |

- [ ] **Step 7:** `cargo test -p komet-ui && cargo test -p komet-engine subagent`.

**Critère :** on peut suivre un sous-agent en direct et rouvrir son transcript
après coup sur l'hôte ; sur un agent qui n'en émet pas, aucune affordance
n'apparaît ; sur un viewer distant, le statut est visible et l'absence de
transcript est annoncée.

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
  texte/nombre, avec `pattern`, `min/max`, `default`.
- Plusieurs propriétés → plusieurs questions paginées (le wizard existant sait
  déjà paginer).
- Réponse : `accept` avec `content` validé, `decline`, `cancel` ; mapper
  `decline`/`cancel` sur `Deny`/annulation de la question, jamais sur un Allow.
- Un schéma inconnu ou non supporté est **préservé** dans la partie et rendu
  comme un refus explicite, pas comme un formulaire approximatif.

**Steps:**
- [ ] **Step 1:** Fixture `elicitation_form_single_select` → une question, deux
  options, une réponse acceptée.
- [ ] **Step 2:** Fixture `elicitation_form_multi_field` → pages, `required`,
  `default`.
- [ ] **Step 3:** Fixture `elicitation_multi_select_array` → labels multiples.
- [ ] **Step 4:** Fixture `elicitation_decline_and_cancel` → pas d'Allow,
  partie résolue avec la bonne issue.
- [ ] **Step 5:** Antigravity : soit le protocole accepte une réponse au tool
  `ask_question` (l'envoyer), soit retirer le bridge et déclarer `None` + notice
  dans le fil. Supprimer `let _ = rx.await;`.
- [ ] **Step 6:** Cursor : conserver `askQuestion` désactivé mais l'exposer dans
  la matrice et l'UI (« Ce harness ne peut pas poser de question »).
- [ ] **Step 7:** UI : la carte de question vit dans le fil + un dock au-dessus
  du composer ; le composer reste actif (steer/texte). Clavier 1–9, Entrée,
  Échap = annuler. Multi-select avec coches. Champ « Autre » si
  `allow_free_text`.
- [ ] **Step 8:** Tests UI : deux questions en attente s'affichent dans l'ordre,
  on peut taper pendant ce temps, et une réponse optimiste se réconcilie avec
  la frame doc.

**Critère :** une question ACP standard (multi-champs, multi-select) est rendue,
répondue, persistée et rejouée ; un harness sans canal n'affiche pas de champ.

---

### Task 7 — Plans et progrès d'outils, par agent

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
  `ToolProgress` (coalescé, ~100 ms, cap 4 Ko côté UI).
- Modify: `crates/ui/src/transcript.rs` : la chip ouverte affiche le flux ;
  une carte Plan persistante peut être ajoutée au-dessus du composer.
- Test: fixtures par agent + test de throttling.

**Par agent :**

| Agent | Plan | Progrès |
| --- | --- | --- |
| Claude Code | `TodoWrite` (déjà) | `None` — ne pas simuler |
| Codex | à mapper (Task 7) | `commandExecution/outputDelta` |
| Cursor | `todos` (déjà) | `Final` |
| Grok / OpenCode / Devin / Copilot / Hermes / Pi / Cline | `plan` ACP (déjà) | `Final` (contenu intermédiaire si présent) |
| Antigravity | `todos` (déjà) | `Final` |
| Muse | à vérifier/mapper | révisions d'item |
| Mock | scénario | scénario |

**Steps:**
- [ ] **Step 1:** `codex_plan_update_maps_to_live_todo`.
- [ ] **Step 2:** `codex_output_delta_is_throttled_and_ephemeral` (aucune
  écriture doc).
- [ ] **Step 3:** `acp_intermediate_tool_content_emits_progress`.
- [ ] **Step 4:** UI : le flux n'apparaît que dans la chip ouverte ; le
  transcript ne re-rend pas.
- [ ] **Step 5:** `cargo test -p komet-harness --test acp && cargo test -p komet-ui`.

**Critère :** ce que le CLI affiche en direct (plan, stdout partiel) est visible
dans Komet quand le protocole l'expose ; sinon la matrice dit `None`/`Final`.

---

### Task 8 — Fluidité de l'interaction

**Gate A (ouverte).** La branche fluidité est mergée (#68, `09b04ed`) :
`perf.rs` et `frame-budget.sh` sont sur `main`. Les compteurs existants
couvrent `Transcript`, `Composer`, `Changes` — pas `ContextUsage` ni le rail, et
aucun compteur par ligne. Step 0 comble ce trou avant toute mesure.

**Files:**
- Modify: `crates/ui/src/perf.rs` (`ChildView::{ContextUsage, Rail}` ; compteur
  de rendu par ligne, clé `(RowKind, id)`).
- Modify: `crates/ui/src/transcript.rs` (instrumentation `row_frame(kind, id)`
  sous `KOMET_WINDOW_STATS`, zéro coût sinon).
- Modify: `crates/ui/src/composer.rs` (fin du takeover ; dock de demandes).
- Modify: `crates/ui/src/transcript.rs` (ancrage de scroll, cartes inline).
- Modify: `crates/ui/src/shell.rs` (activity rail si retenu).
- Modify: `crates/ui/src/state.rs` (file de demandes, activité live).
- Test: `crates/ui` (composer actif pendant une demande, scroll stable,
  compteurs).

**Steps:**
- [ ] **Step 0:** `per_view_and_per_row_counters_exist` : une frame avec
  `context_usage` et deux lignes distinctes incrémente chaque compteur une
  fois, et n'incrémente pas les voisins. Les compteurs sont inertes quand
  `KOMET_WINDOW_STATS` est absent.
- [ ] **Step 1:** `composer_stays_usable_while_a_question_is_pending` : on peut
  saisir et envoyer un steer.
- [ ] **Step 2:** `two_pending_requests_are_ordered_and_keyboard_navigable`.
- [ ] **Step 3:** `compaction_row_does_not_move_the_viewport_when_at_bottom`
  (et ne force pas le scroll si l'utilisateur est remonté).
- [ ] **Step 4:** `approval_timeout_shows_a_countdown_then_auto_denied` (le
  watchdog devient visible).
- [ ] **Step 5:** Capture `scripts/frame-budget.sh --seconds 20` avec un
  scénario mock de compaction : `composer = 0`, `changes = 0`,
  `context_usage ≤ 1`, et seules la ligne de compaction et la jauge se
  re-rendent ; les autres lignes restent à 0.

**Critère :** I3 et I5 sont vrais et **mesurés par vue/ligne** ; aucune
régression du chantier fluidité.

---

### Task 9 — Durabilité et multi-device

**Files:**
- Modify: `crates/engine/src/doc_host.rs` (états « run terminé » explicites).
- Modify: `crates/ui/src/composer.rs` / `transcript.rs` (messages).
- Test: `crates/engine/tests/restart_resume.rs`, `crates/engine/tests/device_routing.rs`.

**Steps:**
- [ ] **Step 1:** `answering_after_restart_starts_a_new_turn_and_says_so`.
- [ ] **Step 2:** `permission_after_restart_is_recorded_but_run_is_gone` — la
  partie affiche « décision enregistrée, run terminé », pas un panneau fantôme.
- [ ] **Step 3:** `concurrent_permission_decisions_keep_deny` (règle existante,
  test multi-device).
- [ ] **Step 3b:** `concurrent_question_answers_converge` : deux appareils
  répondent hors ligne, la fusion élit le même gagnant (§1.5) et un seul
  conflit est affiché.
- [ ] **Step 4:** `compaction_metadata_survives_a_viewer_reconnect` :
  statut/tokens/trigger visibles partout ; le texte du résumé n'apparaît que
  si `SummaryInDoc`, sinon « disponible sur l'hôte ». `subagent_status`
  terminal visible partout ; le transcript distant affiche l'absence.
- [ ] **Step 5:** `cargo test -p komet-engine restart_resume`.

**Critère :** I2 est vrai sur les trois surfaces (question, permission,
compaction) ; pour le sous-agent, statut/durée/compteur survivent et l'absence
de transcript distant est annoncée.

---

### Task 10 — Observabilité, documentation, garde-fous

**Files:**
- Modify: `crates/engine/src/sessions.rs` (compteurs par harness :
  reçus/affichés/droppés ; log debug, pas de spam).
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
- [ ] **Step 1:** Test `no_silent_drop_without_a_surface_entry` : tout `continue`
  de drop d'un événement (twins, méthodes inconnues, compaction) doit être
  justifié par une entrée de matrice ou un log catégorisé.
- [ ] **Step 2:** Golden fixtures anonymisées par agent sous
  `crates/harness/tests/fixtures/` (une par famille de signal).
- [ ] **Step 3:** Mettre à jour les docs ci-dessus (français pour les plans,
  anglais pour `ARCHITECTURE.md`/`PARITY.md` comme le fichier).
- [ ] **Step 4:** `cargo fmt --all && cargo check --workspace --all-targets &&
  cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`.

**Critère :** un nouvel adaptateur ne peut pas shipper sans déclaration ni
fixture ; la doc ne décrit plus un comportement qui n'existe pas.

---

## 4. Tests et fixtures

- **Harness** : une fixture par **famille de signal** (compaction Claude,
  frontière Codex, compact Cursor, update ACP, item Muse, question, plan,
  progrès) ; les golden fixtures par agent sont ajoutées au fur et à mesure
  des vérifications, pas déclarées d'avance. Les fixtures vivent dans
  `crates/harness/tests/fixtures/` et sont parsées, pas exécutées.
- **Engine** : `subagents.rs` (projection journal), `restart_resume.rs`,
  `device_routing.rs`, `turn_quiesce.rs` (une compaction en cours ne doit pas
  être lue comme un tour terminé), politique de résumé.
- **Doc** : round-trip vieux doc, cap du résumé, convergence des réponses
  concurrentes, entrée `System`.
- **UI** : rows, file de demandes, onglet sous-agent, scroll, compteurs de
  rendu par vue/ligne.
- **Manuel** : `scripts/dev-demo.sh --slow` + scénarios mock
  `compaction`, `subagent_nested`, `question_multi`. `scripts/frame-budget.sh`
  (sur `main` depuis #68) pour vérifier qu'aucune ligne ne re-rend le
  transcript.

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

## 6. Séquencement et PRs

| PR | Contenu | Estimation |
| --- | --- | --- |
| 1 | Task 0 — surface + capacités ACP | 0,5–1 j |
| 2 | Task 1 — réponses/décisions visibles | 1–2 j |
| 3 | Task 2 + 3.1 + 3.2 + 3.3 + 3.6 — socle compaction + Claude/Codex/Cursor/Muse | 3–4 j |
| 4 | Task 3.4 + 3.5 + 3.7 — famille ACP (chemin partagé), Antigravity (sonde), Mock | 2–3 j |
| 5 | Task 4 — projection journal sous-agent + RPC | 2–3 j |
| 6 | Task 5 — UI sous-agent | 2–3 j |
| 7 | Task 6 — questions | 2–3 j |
| 8 | Task 7 — plans/progrès | 2 j |
| 9 | Task 9 — durabilité (indépendante) | 2 j |
| 10 | Task 8 — fluidité (compteurs par vue/ligne étendus) | 2–3 j |
| 11 | Task 10 — observabilité/docs | 1 j |

Total : ~20–28 jours à une personne, en PRs indépendantes. Les PRs 3 et 5
peuvent avancer en parallèle (fichiers disjoints) ; la PR 6 dépend de 5 ; la
PR 10 peut être la dernière.

## 7. Definition of Done par agent

| Agent | Compaction visible | Sous-agent visible | Questions répondues + persistées | Plan | Progrès | No-op silencieux |
| --- | --- | --- | --- | --- | --- | --- |
| Claude Code | oui (`status` + frontière + résumé) | oui | oui (natif) | oui | déclaré `None` | supprimé |
| Codex | oui (frontière) | oui | non (approvals seulement, déclaré) | oui | oui (streaming) | supprimé |
| Cursor | oui (frontière) | oui | non (déclaré) | oui | final | supprimé |
| Grok | oui si Gate B validée et update observé | oui (tail, hôte) | oui (elicitation) ou fallback déclaré | oui | final | supprimé |
| OpenCode | oui si Gate B validée et update observé | oui (hôte) | oui/fallback | oui | final | supprimé |
| Devin | oui si Gate B validée et update observé | selon émission, déclaré | oui/fallback | oui | final | supprimé |
| Copilot | idem Devin | idem | idem | oui | final | supprimé |
| Hermes | idem | idem | idem | oui | final | supprimé |
| Pi | idem | idem | idem | oui | final | supprimé |
| Cline | idem | idem | idem | oui | final | supprimé |
| Antigravity | déclaré `None` si aucun signal | déclaré `None` | réparé ou déclaré `None` | oui | final | supprimé |
| Muse | oui (item démasqué) | oui | oui (natif) | oui | oui | supprimé |
| Mock | oui | oui | oui | oui | oui | — |

Chaque ligne « déclaré » doit être couverte par `interaction_surfaces_are_declared`
et par une ligne visible dans Settings → Agents.

Lecture des colonnes : **Compaction visible** = métadonnées dans le doc sur
tous les appareils + résumé fetchable sur l'hôte (sauf `SummaryInDoc`) ;
**Sous-agent visible** = transcript sur l'hôte, statut/durée/compteur terminal
partout. Aucune ligne ACP ne peut passer « oui » avant la validation réelle de
Gate B ; d'ici là elle reste `None` et l'affordance est masquée.
