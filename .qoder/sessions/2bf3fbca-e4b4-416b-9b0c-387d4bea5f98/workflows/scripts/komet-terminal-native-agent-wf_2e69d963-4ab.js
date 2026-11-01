export const meta = {
  name: 'komet-terminal-native-agent',
  description: "Rendre le terminal de Komet natif pour l'humain et lisible/pilotable par les agents",
  phases: [
    { title: 'Design', detail: "un brief d'implémentation ancré dans le code par sous-système" },
    { title: 'Build', detail: 'implémentation TDD dans un worktree git isolé par sous-système' },
    { title: 'Merge', detail: 'fusion séquentielle des branches, résolution de conflits, gates par crate' },
    { title: 'Verify', detail: 'vérification adversariale: la feature est réellement câblée et testée' },
    { title: 'Gates', detail: 'critique de complétude puis clippy/format/sécurité' },
  ],
}

const REPO = '/home/jomvick/Bureau/Kc_Folder/projet/komet'

const ANCHORS = `
Ancrages vérifiés dans le dépôt ${REPO} (relis-les, ils peuvent avoir bougé):
- crates/engine/src/terminals.rs — vrais PTY via portable-pty; pump_output ne garde que des OCTETS BRUTS (fenêtre 1 Mo base64); resize: MasterPty::resize vers la ligne 282-293, clamp_size=100; cap à 32 terminaux ligne 30.
- crates/ui/src/terminal/emulator.rs (728 l.) — cœur alacritty_terminal: SCROLLBACK_LINES = 10_000 figé ligne 44; take_bell() existe ligne 205/573 mais aucun appelant; feed() JETTE les événements OSC non-titre dans un "_ => {}" ligne 200-208; modes exposés = app-cursor + bracketed-paste seulement ligne 235-242; wide+spacer ligne 351-354; sélections ligne 304-320.
- crates/ui/src/terminal/view.rs (1270 l.) — TERM_FONT_SIZE=13.0 / TERM_LINE_HEIGHT=18.0 figés ligne 27-28; font = theme.font_mono ligne 534; paste_bytes(text, bracketed) ligne 410; pas de hit-test URL dans cell_at (ligne 248).
- crates/ui/src/terminal/panel.rs (1783 l.) — on_mouse_down/move/up ligne 950-1056 font TOUJOURS de la sélection locale (jamais de relais au PTY); menu contextuel = 2 entrées Copy/Paste ligne 1137-1152; copy_selection ligne 1165; raccourci unique Cmd/Ctrl+J ligne 44-53; bouton "+" souris-only ligne 1478; titre d'onglet = OSC title sinon "Terminal N" ligne 372-377; shell_title()/cwd calculés mais morts ligne 154; ResizeTerminal debouncé 80 ms ligne 851-905; clic du milieu pris par la fermeture d'onglet ligne 1407.
- crates/ui/src/terminal/mod.rs — 16 lignes, sorties publiques du module.
- crates/engine/src/mcp/catalog.rs — catalogue actuel: list_sessions, get_session_status, list_spaces; TOOL_TIMEOUT 30 s; 4 appels simultanés max; passe par le trait McpHost.
- crates/engine/src/mcp/permissions.rs — système de demande de permission déjà en place pour les outils MCP.
- crates/harness/src/claude/normalize.rs:89 — "Bash" => ToolCall::Exec DÉJÀ mappé.
- crates/harness/src/codex/normalize.rs:273 — "commandExecution" => ToolCall::Exec; aggregatedOutput à la fin; le delta vit sur "item/commandExecution/outputDelta" (ligne 391/532).
- crates/harness/src/claude/permissions.rs:59-77 — construit déjà --allowed-tools / --disallowed-tools.
- crates/ui/src/shell.rs:270, 2640-2654 — RightSurface::Terminal(u64) existe déjà (panneau droit).
- crates/engine/src/settings.rs:141-143 — paramètres terminal actuels: terminal_height/terminal_open uniquement.
- Shell lancé avec TERM_PROGRAM=Komet (à vérifier dans engine/terminals.rs).
`

const CONVENTIONS = `
Conventions du projet (AGENTS.md, à respecter strictement):
- Rust edition 2024, stable. Dépendances et versions déclarées UNE fois dans le Cargo.toml racine, les crates référencent { workspace = true }.
- Ne PAS compiler le workspace entier ni toutes les cibles: gpui est très lent. Utiliser cargo check -p <crate> / cargo test -p <crate> <nom-du-test>. Un par un, avec le nom du test.
- Avant de conclure: cargo fmt --all, puis clippy -D warnings sur les crates touchées, puis les tests des crates touchées.
- Beaucoup de tests sont #[ignore]d (serveur ou CLI agent réel). Ne pas les lever pour une validation locale.
- Commits Conventionnal Commits avec scope, ex: fix(terminal): ...
- Toute nouvelle capacité de harness doit déclarer sa ligne dans le matrix §6 d'ARCHITECTURE.md ET sa ligne d'interaction-surface dans crates/harness/src/capabilities.rs (golden test crates/harness/tests/surfaces.rs). Un no-op silencieux est interdit.
- Les réglages sandbox Komet contraignent les permissions du CLI agent lui-même, pas une frontière OS: lire docs/security.md avant de toucher permission/sandbox.
`

const BRIEF_SCHEMA = {
  type: 'object',
  properties: {
    subsystem: { type: 'string' },
    objective: { type: 'string' },
    files_to_create: { type: 'array', items: { type: 'string' } },
    files_to_modify: { type: 'array', items: { type: 'string' } },
    steps: {
      type: 'array',
      items: {
        type: 'object',
        properties: {
          title: { type: 'string' },
          detail: { type: 'string' },
          target: { type: 'string', description: 'fichier:ligne ou symbole exact à toucher' },
          test: { type: 'string', description: 'nom du test qui prouve ce pas' },
        },
        required: ['title', 'detail', 'target'],
      },
    },
    tests: { type: 'array', items: { type: 'string' }, description: 'tests à écrire AVANT le code, avec leurs noms exacts' },
    acceptance: { type: 'array', items: { type: 'string' }, description: "critères observables, mesurables à l'œil ou par un test" },
    risks: { type: 'array', items: { type: 'string' } },
    open_questions: { type: 'array', items: { type: 'string' } },
    depends_on: { type: 'array', items: { type: 'string' }, description: 'sous-systèmes dont ce brief suppose le résultat' },
    crate_targets: { type: 'array', items: { type: 'string' } },
    estimated_steps: { type: 'number' },
  },
  required: ['subsystem', 'objective', 'files_to_modify', 'steps', 'tests', 'acceptance', 'risks', 'crate_targets'],
}

const BUILD_SCHEMA = {
  type: 'object',
  properties: {
    branch: { type: 'string', description: 'nom de la branche du worktree, rev-parse --abbrev-ref HEAD' },
    worktree_path: { type: 'string' },
    commit_count: { type: 'number' },
    crate_targets: { type: 'array', items: { type: 'string' } },
    implemented: { type: 'array', items: { type: 'string' } },
    skipped: { type: 'array', items: { type: 'string' }, description: 'pas du brief NON faits, avec la raison' },
    tests_added: { type: 'array', items: { type: 'string' } },
    checks_run: { type: 'array', items: { type: 'string' }, description: 'commandes réellement exécutées + verdict' },
    files_touched: { type: 'array', items: { type: 'string' } },
    notes_for_merge: { type: 'array', items: { type: 'string' }, description: 'risques de conflit, fichiers chauds' },
    self_verdict: { type: 'string', enum: ['complete', 'partial', 'blocked'] },
  },
  required: ['branch', 'worktree_path', 'implemented', 'skipped', 'checks_run', 'files_touched', 'self_verdict'],
}

const VERDICT_SCHEMA = {
  type: 'object',
  properties: {
    claim: { type: 'string' },
    is_real: { type: 'boolean', description: 'true = la feature existe et est câblée; false = claim non étayée' },
    evidence: { type: 'array', items: { type: 'string' }, description: "fichier:ligne + ce qu'on y voit" },
    unwired: { type: 'array', items: { type: 'string' }, description: 'symboles écrits mais jamais appelés depuis le code de production' },
    silent_no_op: { type: 'boolean' },
    tests_genuine: { type: 'boolean', description: 'les tests prouvent le comportement, pas seulement la compilation' },
    severity: { type: 'string', enum: ['none', 'minor', 'major', 'blocking'] },
    recommendation: { type: 'string' },
  },
  required: ['claim', 'is_real', 'evidence', 'unwired', 'tests_genuine', 'severity'],
}

const GATE_SCHEMA = {
  type: 'object',
  properties: {
    passed: { type: 'boolean' },
    commands: { type: 'array', items: { type: 'string' } },
    failures: { type: 'array', items: { type: 'string' } },
    fixes_applied: { type: 'array', items: { type: 'string' } },
  },
  required: ['passed', 'commands', 'failures'],
}

const MERGE_SCHEMA = {
  type: 'object',
  properties: {
    merged: { type: 'array', items: { type: 'string' } },
    rejected: { type: 'array', items: { type: 'object', properties: { branch: { type: 'string' }, reason: { type: 'string' } }, required: ['branch', 'reason'] } },
    conflicts: { type: 'array', items: { type: 'string' }, description: 'fichier + comment le conflit a été résolu' },
    checks: { type: 'array', items: { type: 'string' } },
    leftovers: { type: 'array', items: { type: 'string' }, description: 'réconciliations à faire après coup' },
  },
  required: ['merged', 'rejected', 'conflicts', 'checks', 'leftovers'],
}

const CRITIC_SCHEMA = {
  type: 'object',
  properties: {
    missing: { type: 'array', items: { type: 'string' }, description: 'fonctionnalité annoncée mais absente ou non câblée' },
    unwired: { type: 'array', items: { type: 'string' } },
    unverified: { type: 'array', items: { type: 'string' }, description: 'acceptance non observée' },
    doc_gaps: { type: 'array', items: { type: 'string' }, description: 'ARCHITECTURE.md §6, capabilities.rs, docs/security.md non mis à jour' },
    next_round: { type: 'array', items: { type: 'string' }, description: 'travail restant priorisé' },
  },
  required: ['missing', 'unwired', 'unverified', 'doc_gaps', 'next_round'],
}

const SUBSYSTEMS = [
  {
    key: 'term-core',
    title: '0 — extraire le cœur émulateur dans un crate partagé sans gpui',
    effort: 'high',
    goal: 'Créer une lib (proposer le nom komet-term) qui contient la grille, le scrollback, la sélection, l alt-screen, le vraicolour et le feed d événements, SANS dépendre de gpui. crates/ui/src/terminal/emulator.rs ne devient que l adaptateur gpui (rendu, couleurs de thème, mesure des glyphes) au-dessus d elle. Le MOTEUR (crates/engine/src/terminals.rs) l alimente depuis pump_output, par terminal, pour les terminaux suivis uniquement (flag d activation par terminal, coût CPU nul pour les autres), et expose trois requetes: écran courant en texte clair, N dernières lignes, delta depuis un seq donné. C est le prérequis de tout le reste: ne rien casser côté UI est le critère d acceptation numéro 1.',
  },
  {
    key: 'mouse-native',
    title: '1+2a — souris relayée au PTY, copy-on-select, clic du milieu PRIMARY',
    effort: 'high',
    goal: 'Exposer les modes souris demandés par l application (DECSET 1000 clic, 1002 trainée, 1003 tout-mouvement, 1006 SGR) depuis l émulateur, encoder clics/molette/drag en octets SGR vers le PTY, et basculer: mode souris actif => relais au PTY, Shift enfoncé => sélection locale forcée; aucun mode actif => comportement actuel (sélection). Ajouter copy-on-select et collation clic du milieu depuis la sélection PRIMARY (le clic du milieu est aujourd hui pris par la fermeture d onglet, panel.rs:1407 — le rendre contextuel: dans la grille = paste, sur l onglet = close). Ces apps TUI doivent devenir utilisables: vim, htop, lazygit, tmux, less.',
  },
  {
    key: 'chrome',
    title: "2b — raccourcis, scrollback et police configurables, cloche visuelle, menu",
    effort: 'medium',
    goal: 'Raccourcis clavier: nouveau terminal, fermer, onglet suivant/précédent, renommer. Étoffer le menu contextuel (aujourd hui 2 entrées): Select All, Search, New, Open link si pertinent. Sortir TERM_FONT_SIZE et SCROLLBACK_LINES des constantes: taille de police plus/moins (Ctrl+= / Ctrl+- / Ctrl+0), famille mono, et capacité de scrollback dans crates/engine/src/settings.rs à côté de terminal_height. Cloche visuelle: take_bell() est déjà capté et jamais affiché — l afficher (flash de l onglet + indicateur) et la rendre optionnellement audible. Titre d onglet: brancher shell_title()/cwd (aujourd hui morts, panel.rs:154) pour afficher cwd et nom de shell quand aucune OSC title n est reçue. Ne PAS toucher à la logique de feed OSC des autres sous-systèmes.',
  },
  {
    key: 'mcp-read',
    title: '3 — terminal_list et terminal_read en lecture seule',
    effort: 'medium',
    goal: 'Ajouter au catalogue MCP (crates/engine/src/mcp/catalog.rs, via le trait McpHost) deux outils en lecture seule: terminal_list (id, titre, cwd, occupé par qui, vivant) et terminal_read (sélecteur: écran / dernières N lignes / delta depuis un seq / dernière commande). Troncation en gardant la FIN des réponses. Pour une app plein écran, renvoyer l instantané d écran, jamais l historique. Ajouter l interrupteur par terminal "visible par les agents", désactivé par défaut pour les terminaux de l humain (un terminal peut afficher des secrets), avec les modes lecture seule / lecture-écriture. Ce sous-système dépend de term-core pour la lecture en texte clair: dans le brief, décrire l interface attendue et la coder contre une implémentation minimale si le crate n est pas encore fusionné.',
  },
  {
    key: 'osc133-run',
    title: '4 — OSC 133: frontières de commande + terminal_run/send_input/open/close',
    effort: 'high',
    goal: 'Injecter les marqueurs OSC 133 au lancement du shell (bash: PROMPT_COMMAND / PS1, zsh: precmd+preexec, fish: fish_prompt hooks; PowerShell = cas séparé à marquer explicitement non fait), et les parser dans le cœur émulateur pour produire des blocs {commande, cwd, sortie, code de retour}, avec repli "silencieux depuis N ms" quand les marqueurs manquent. Double consommateur: navigation commande-précédente/suivante dans l UI ET blocs pour les agents. Puis les outils MCP d écriture: terminal_run (exécute et attend la fin), terminal_send_input (touches brutes, Ctrl-C), terminal_open, terminal_close — TOUS derrière le système de permission existant (crates/engine/src/mcp/permissions.rs). Gérer le TOOL_TIMEOUT de 30 s et les 4 appels simultanés: une commande longue renvoie immédiatement un identifiant et l agent relit avec terminal_read.',
  },
  {
    key: 'coexist-external',
    title: "5 — copropriété UI + visibilité des shells d'agents externes",
    effort: 'medium',
    goal: 'Un terminal a un propriétaire: l humain ou un agent. Les terminaux d agent apparaissent comme onglets du panneau droit (RightSurface::Terminal existe déjà, shell.rs:270) avec l icône de l agent. Pendant qu un agent pilote, la saisie de l humain est verrouillée avec un bouton "Prendre la main" explicite. Dans la conversation, la ligne d outil "a lancé cargo test" devient un lien vers l onglet. Deuxième volet: Claude Code et Codex exécutent leur outil shell chez eux, mais ToolCall::Exec est DÉJÀ mappé (claude/normalize.rs:89, codex/normalize.rs:273) — intercepter ces événements et les afficher dans un onglet "Shell agent" en lecture seule pour TOUS les harness, en branchant le delta item/commandExecution/outputDelta de Codex pour du flux live. Marquer explicitement ce qui reste non fait pour le vrai terminal partagé (--disallowed-tools Bash déjà constructible, claude/permissions.rs:71).',
  },
]

// ── Phase Design: fan-out pur; les briefs sont lus par les builders ──
phase('Design')
const briefs = (await parallel(SUBSYSTEMS.map(s => () => agent(
  `Tu es architecte sur le dépôt Komet (${REPO}), un workspace Rust edition 2024 avec gpui.

TÂCHE: produire un BRIEF D'IMPLÉMENTATION pour le sous-système ci-dessous. Tu ne codes rien. Tu lis le code réel et tu rends un plan exécutable par un autre agent, pas à pas, avec les cibles exactes et les tests à écrire avant le code.

SOUS-SYSTÈME ${s.key}: ${s.title}
${s.goal}

${ANCHORS}
${CONVENTIONS}

MÉTHODE OBLIGATOIRE:
1. Lis les fichiers ancrés ci-dessus AVANT d'écrire quoi que ce soit. Chaque cible de chaque pas doit être un fichier:ligne ou un symbole que tu as VU. Pas de ligne inventée.
2. Si un ancrage est faux ou a bougé, corrige-le dans ton brief et signale-le dans risks.
3. Les tests listés doivent être nommés et leur placement décrit (quel fichier, unitaires de lib ou crates/*/tests/). Souviens-toi que crates/ui n'a que des tests unitaires de lib — la validation UI passe par cargo check -p komet-ui et scripts/dev-demo.sh.
4. Nomme les questions ouvertes franchement plutôt que d'inventer une réponse. Ne planifie pas ce qui relève d'un autre sous-système, mais liste tes dépendances dans depends_on.
5. Le critère d'acceptation doit être observable: un test qui passe, ou un comportement à vérifier à l'œil dans le terminal (vim, htop...).

Remplis le StructuredOutput. subsystem = "${s.key}".`,
  { label: `brief:${s.key}`, phase: 'Design', schema: BRIEF_SCHEMA, effort: 'high' },
)))).filter(Boolean)

log(`${briefs.length}/${SUBSYSTEMS.length} briefs produits`)

const briefByKey = {}
for (const b of briefs) briefByKey[b.subsystem] = b

// ── Phase Build: worktrees isolés, un agent par sous-système ──
phase('Build')
const builds = (await parallel(SUBSYSTEMS.map(s => () => {
  const brief = briefByKey[s.key] || null
  return agent(
    `Tu implémentes le sous-système "${s.key}" — ${s.title} — dans le dépôt Komet (${REPO}).

Tu travailles dans ton WORKTREE GIT ISOLÉ (déjà créé pour toi; vérifie avec pwd et git status au départ). Ne fusionne rien, ne pousse RIEN sur aucune remote, ne touche pas à la branche principale. Travailler dans le worktree est ta seule écriture autorisée.

BRIEF D'ARCHITECTE (produit par un agent de design à partir du même code — suis-le, et dévie-le seulement si le code réel lui donnait tort, en le notant dans skipped):
${brief ? JSON.stringify(brief, null, 2) : '(aucun brief — débrouille-toi à partir de la description du sous-système ci-dessus)'}

OBJECTIF: ${s.goal}

${ANCHORS}
${CONVENTIONS}

MÉTHODE OBLIGATOIRE — TDD strict:
1. Écris D'ABORD les tests du brief (rouge), puis le code (vert). Un pas de logique sans test n'existe pas; les pas purement visuels se valident par cargo check -p komet-ui plus la description du comportement attendu dans acceptance.
2. Compile avec les commandes COURTES imposées par AGENTS.md: cargo test -p <crate> <nom-du-test>, cargo check -p komet-ui. Ne lance JAMAIS cargo check --workspace --all-targets ni un build de tout le workspace — gpui est lent et d'autres agents tournent en parallèle sur la même machine.
3. Commit sur ta branche au fil de l'eau, messages Conventionnal Commits avec scope.
4. Si un pas du brief est hors de portée ou dépend d'un autre sous-système non encore fusionné: saute-le et écris-le dans skipped avec la raison. Ne simule PAS une fonctionnalité. "Un no-op silencieux est interdit" est la règle du projet: si tu n'as pas le temps de câbler, dis-le au lieu de poser du code mort.
5. cargo fmt --all et clippy -D warnings sur les crates que tu as touchées avant de rendre.

Remplis le StructuredOutput. branch = sortie de git rev-parse --abbrev-ref HEAD. checks_run = les commandes que tu as RÉELLEMENT exécutées avec leur verdict (ne liste rien que tu n'aies lancé).`,
    { label: `build:${s.key}`, phase: 'Build', schema: BUILD_SCHEMA, effort: s.effort === 'high' ? 'high' : 'medium', isolation: 'worktree', agentType: 'general-purpose' },
  )
}))).filter(Boolean)

log(`${builds.length}/${SUBSYSTEMS.length} branches rendues; verdicts auto: ${builds.map(b => `${b.branch}=${b.self_verdict}`).join(', ')}`)

// ── Phase Merge: barrière légitime — un seul agent séquentiel sur l'arbre partagé ──
phase('Merge')
const merge = await agent(
  `Tu fusionnes les branches produites par plusieurs agents de build dans la branche principale de Komet (${REPO}). Les branches sont issues de worktrees isolés et ont toutes été créées depuis le même commit de base; elles se chevauchent forcément sur crates/ui/src/terminal/* et crates/engine/src/terminals.rs.

BRANCHES (avec les fichiers touchés et les avertissements de chacun):
${JSON.stringify(builds.map(b => ({ branch: b.branch, worktree_path: b.worktree_path, verdict: b.self_verdict, files_touched: b.files_touched, notes_for_merge: b.notes_for_merge, skipped: b.skipped })), null, 2)}

${CONVENTIONS}

ORDRE DE FUSION: term-core d'abord, puis mouse-native, chrome, mcp-read, osc133-run, coexist-external — les suivants supposent le crate partagé du premier.

RÈGLES DE SÉCURITÉ GIT — non négociables:
- Jamais de --no-verify, jamais de push, jamais de reset --hard, jamais de git clean. Ne détruis le travail d'aucun agent.
- Lance git status AVANT toute fusion et respecte-le: s'il y a des modifications non commitées dans l'arbre principal, arrête-toi et signale-les dans leftovers plutôt que de les écraser.
- Fusionne une branche à la fois (git merge --no-ff) et, entre deux, lance les gates COURTS sur les crates que la fusion a touchés: cargo check -p komet-ui, cargo test -p komet-engine <tests pertinents>. Pas de build workspace.
- Résous les conflits en CONSERVANT les deux intentions, pas en jetant un côté. Si un conflit est structurel (deux agents ont refactoré la même fonction), préfère la version du crate partagé (term-core) et reporte l'autre changement par-dessus.
- Si une branche casse la compilation et que tu ne peux pas la réparer proprement dans cette session: dé-fusionne cette branche seule (git revert du commit de merge), indique-le dans rejected avec la raison, et passe à la suivante. Mieux vaut une feature en moins qu'un main cassé.
- Commit de merge explicite par branche.

Remplis le StructuredOutput. Sois honnête dans conflicts: liste chaque fichier où tu as dû arbitrer et comment.`,
  { label: 'merge:sequential', phase: 'Merge', schema: MERGE_SCHEMA, effort: 'high' },
)

if (!merge) throw new Error('fusion interrompue — aucun résultat de merge')
log(`fusionnées: ${merge.merged.length} · rejetées: ${merge.rejected.length} · conflits arbitrés: ${merge.conflicts.length}`)

// ── Phase Verify: adversarial, sur le main fusionné ──
phase('Verify')
const LENSES = [
  { lens: 'câblage réel', ask: "Cherche les symboles produits et prouve qu'ils sont APPELÉS depuis le code de production, pas seulement testés par leurs propres tests. Un take_bell() capté mais jamais affiché est exactement le motif à débusquer." },
  { lens: 'tests authentiques', ask: 'Les tests prouvent-ils le COMPORTEMENT ou seulement la compilation? Essaie de rendre le code faux (inverser une condition, couper un relais) et dis si un test échouerait.' },
  { lens: 'régression humaine', ask: 'Est-ce que le comportement qui marchait avant est encore là: sélection locale, bracketed paste, reflow au resize, alt-screen, onglets, copier? Un relais souris au PTY qui avale la sélection est une régression bloquante.' },
]

const verdicts = (await parallel(SUBSYSTEMS.map((s, i) => () => {
  const mergedBranch = builds.find(b => b && (b.branch || '').includes(s.key))
  const l = LENSES[i % LENSES.length]
  return agent(
    `Tu es un vérificateur ADVERSAIRE sur Komet (${REPO}), branche principale après fusion. Ta posture: essaie de RÉFUTER. Le doute se règle en défaveur de la claim.

SOUS-SYSTÈME À AUDITER: "${s.key}" — ${s.title}
CE QUI ÉTAIT PROMIS: ${s.goal}
CE QUE LE BUILDER DÉCLARE FAIT: ${JSON.stringify(mergedBranch ? mergedBranch.implemented : '(inconnu)')}
CE QU'IL ADMET NON FAIT: ${JSON.stringify(mergedBranch ? mergedBranch.skipped : '(aucun aveu — méfie-t-en d\'autant plus)')}

LENS DE CETTE PASSE — ${l.lens}: ${l.ask}

CONSIGNES:
- Le code est sur main, les worktrees ont pu être nettoyés. Audite l'état fusionné, pas les promesses.
- Lis le code, lance les tests pertinents avec les commandes COURTES (-p <crate> <nom-du-test>). Ne lance pas un build workspace.
- Remplis unwired avec tout symbole de production que tu ne trouves pas dans la chaîne d'appel. C'est le défaut connu du dépôt (bell, shell_title, événements OSC jetés dans un "_ => {}").
- silent_no_op = true si une fonctionnalité annoncée existe en code mais ne produit aucun effet observable pour l'utilisateur ou pour un agent.
- Sois précis dans evidence: fichier:ligne + ce que tu vois.
- recommendation = la prochaine action, en une phrase.
- claim = une phrase qui résume ce que tu as jugé.`,
    { label: `verify:${s.key}`, phase: 'Verify', schema: VERDICT_SCHEMA, effort: 'high' },
  )
}))).filter(Boolean)

const refuted = verdicts.filter(v => !v.is_real || v.silent_no_op || v.severity === 'blocking' || v.severity === 'major')
log(`${verdicts.length} verdicts · ${refuted.length} contestés`)

// ── Phase Gates: complétude + sécurité + hygiène ──
phase('Gates')
const [critic, security, hygiene] = await parallel([
  () => agent(
    `Tu es le critique de COMPLÉTUDE du projet terminal de Komet (${REPO}). L'ambition déclarée: un terminal aussi capable qu'un terminal natif (GNOME Terminal/iTerm2/VS Code) ET lisible/pilotable par les agents MCP.

BRIEFS ORIGINAUX: ${JSON.stringify(briefs.map(b => ({ subsystem: b.subsystem, acceptance: b.acceptance })), null, 2)}
VERDICTS ADVERSAIRES: ${JSON.stringify(verdicts, null, 2)}
ÉTAT DE LA FUSION: ${JSON.stringify(merge, null, 2)}

Question à laquelle tu réponds: QU'EST-CE QUI MANQUE? Pas ce qui est bien. Cherche:
- une acceptance de brief non observée dans le code fusionné;
- une feature écrite mais non câblée à l'UI ou au catalogue MCP;
- une capacité de harness nouvelle qui n'a pas sa ligne dans le matrix §6 d'ARCHITECTURE.md NI dans crates/harness/src/capabilities.rs (golden test tests/surfaces.rs) — règle du projet: un no-op silencieux est interdit;
- docs/security.md non mis à jour alors que terminal_run expose une surface d'exécution;
- le cas Windows/PowerShell pour OSC 133, le routage multi-appareils sync vers l'appareil propriétaire du terminal, le scrollback des sessions longues, les apps plein écran qui doivent renvoyer l'instantané et non l'historique;
- le chemin d'amélioration évident que personne n'a pris.

Priorise next_round: ce qui débloque le plus pour un effort donné, en premier.`,
    { label: 'gate:completeness', phase: 'Gates', schema: CRITIC_SCHEMA, effort: 'high' },
  ),
  () => agent(
    `Tu audits la SÉCURITÉ du terminal de Komet (${REPO}) après l'ajout des outils MCP de pilotage. Contexte projet obligatoire: les réglages sandbox Komet contraignent les permissions du CLI agent lui-même, PAS une frontière OS — lis docs/security.md d'abord.

Surface à attaquer:
- crates/engine/src/mcp/catalog.rs et permissions.rs: terminal_run / terminal_send_input / terminal_open / terminal_close sont-ils TOUS derrière une demande de permission? Un outil d'écriture qui a échappé au garde est un trou bloquant.
- L'interrupteur "visible par les agents", par défaut désactivé pour les terminaux de l'humain: prouve que le défaut est bien off et qu'un agent ne peut pas le lever lui-même.
- terminal_read: fuite de secrets (le scrollback contient tokens, mots de passe, clés). Que renvoie un read sur un terminal qui vient d'afficher un secret?
- Injection: un agent qui envoie des touches brutes peut-il échapper à la session; une sortie ANSI piégée peut-elle détourner un rendu ou un read?
- Race de copropriété: humain qui reprend la main pendant qu'un agent écrit — les deux saisies se mélangent-elles?
- Claude/Codex: --disallowed-tools Bash (claude/permissions.rs:59-77) sert-il de levier pour forcer terminal_run, et si oui l'agent peut-il contourner (autres outils shell, subagents, hooks, MCP externe)?
- TOOL_TIMEOUT 30 s et 4 appels simultanés: un agent peut-il créer un amas de commandes longues non suivies?

Rends un verdict par issue avec sévérité et la correction minimale. claim = "surface d'exécution agent-terminale sous garde". Ne modifie AUCUN fichier: tu signales, tu ne répares pas.`,
    { label: 'gate:security', phase: 'Gates', schema: VERDICT_SCHEMA, effort: 'high' },
  ),
  () => agent(
    `Tu passes les gates d'HYGIÈNE sur Komet (${REPO}) après une fusion multi-branches, et tu corriges ce qui ne peut pas attendre.

Dans cet ordre, avec les commandes exactes:
1. cargo fmt --all
2. cargo clippy --workspace --all-targets -- -D warnings   ← cette fois le workspace est légitime: c'est le gate de fin, plus aucun autre agent ne tourne.
3. cargo test --workspace
4. cargo check --workspace --all-targets

Si une commande échoue: diagnostique et corrige la CAUSE (ne contourne pas avec un allow(), ne supprime pas un test pour le rendre vert, n'utilise jamais --no-verify). Si une correction dépasse le détail de formatage ou de lint et touche à la logique d'une feature, arrête-toi et décris-la dans failures au lieu de bricoler.
Liste dans commands chaque commande lancée et son verdict réel. passed = true seulement si les quatre sont vertes.`,
    { label: 'gate:hygiene', phase: 'Gates', schema: GATE_SCHEMA, effort: 'medium' },
  ),
])

return {
  briefs: briefs.map(b => ({ subsystem: b.subsystem, steps: b.steps.length, acceptance: b.acceptance })),
  builds: builds.map(b => ({ branch: b.branch, verdict: b.self_verdict, done: b.implemented, skipped: b.skipped })),
  merge,
  verdicts,
  contested: refuted.map(v => ({ claim: v.claim, severity: v.severity, unwired: v.unwired, recommendation: v.recommendation })),
  critic,
  security,
  hygiene,
}