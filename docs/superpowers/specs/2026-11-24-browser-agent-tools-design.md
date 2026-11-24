# Outils agent vers le Browser visible — Design (Phase 3)

Date : 2026-11-24. Statut : implémenté (v1). Remplace, pour la surface visible, le
chemin `chrome-devtools-mcp --headless --isolated` de la spec 2026-10-12 (qui reste un
prototype acceptable pour l'automatisation sans interface). La matrice de parité vit
dans `docs/PARITY.md` §3.6.

## 1. Principe

L'agent pilote **la surface Browser que l'utilisateur voit** — jamais un Chrome
parallèle. La chaîne :

```
agent (outil MCP `browser_*`)
  → pont moteur (`BrowserBridge` : file de jobs + attente du résultat)
  → RPC IPC (`PollBrowserJobs` / `BrowserJobResult`, locaux uniquement)
  → exécuteur UI (surface Browser ouverte, poll 300 ms)
  → session CDP (commandes existantes de `browser/session.rs`)
  → résultat remonté au même job
```

La surface visible reste l'autorité : un job ne s'exécute que si une surface Browser
est ouverte ; sinon l'outil répond une erreur explicite (« no visible Browser surface
is open ») — jamais un succès silencieux, jamais un Chrome lancé en cachette.

## 2. Contrat

### 2.1 Jobs (JSON, pas de type proto dédié)

- `BrowserJob { id, tool, args }` — `id` = `job-<n>` attribué par le pont.
- Résultat RPC `{ jobId, ok, value }` — `value` est le JSON de réponse de l'outil ;
  `ok=false` → le message d'erreur devient l'erreur de l'outil.

### 2.2 RPC (komet_rpc::methods, IPC-only, jamais relay-forwarded)

- `PollBrowserJobs {max?}` → `{jobs: [BrowserJob]}` — draine la file (8 par défaut, 32 max).
- `BrowserJobResult {jobId, ok, value}` → `{}` — complète le handler en attente.

Les deux sont dans `LOCAL_ONLY` : une connexion d'environnement ne pilote pas le
navigateur visible de cette machine.

### 2.3 Outils MCP (serveur `komet` interne)

| Outil | Args | Lecture seule | Réponse |
|---|---|---|---|
| `browser_navigate` | `{url}` | non | `{url}` |
| `browser_snapshot` | `{}` | oui | `{url, title, snapshot, elements: [{uid, role, text, tag, selector, x, y, width, height}]}` |
| `browser_click` | `{uid, snapshot?}` | non | `{clicked: uid}` |
| `browser_fill` | `{uid, text, snapshot?}` | non | `{filled: uid}` |
| `browser_press_key` | `{key}` | non | `{key}` |
| `browser_wait` | `{ms ≤ 10000}` | oui | `{waited: ms}` |
| `browser_screenshot` | `{}` | non | `{path}` (fichier dans le dossier de téléchargements ; les pixels ne quittent jamais la machine) |
| `browser_console` | `{level?}` | oui | `{entries: [{level, text}]}` |

`readonly=false` → la politique MCP existante demande une confirmation humaine
(`Decision::Ask`) pour navigate/click/fill/press_key/screenshot — le canal
d'approbation habituel sert de confirmation explicite avant action sensible.

## 3. Règles de sécurité

- **CDP jamais exposé** : le port WebSocket reste sur loopback et derrière la
  surface ; les agents ne voient que ces outils typés.
- **Profils** : l'espace de noms `agent:` (`process::agent_profile_key`, testé) est
  réservé aux sessions d'agent sans cookies de l'utilisateur. La v1 pilote la
  surface visible (profil humain, transparent pour l'utilisateur) ; un jour où un
  agent aurait ses propres onglets, ils naîtraient sur `agent_profile_key`.
- **Contenu de page non fiable** : les résultats (`text`, console) traversent le
  canal MCP comme du texte, jamais injectés dans des journaux persistants au-delà
  du transcript du run.
- **UID éphémères, épinglés au snapshot** : valides pour le snapshot qui les a
  créés (génération comptée côté surface, renvoyée dans la réponse de
  `browser_snapshot` sous `snapshot`). Changer de document invalide la liste
  (navigation → liste vidée). `browser_click`/`browser_fill` acceptent un
  argument `snapshot` optionnel : s'il ne correspond pas à la génération
  courante, l'action est refusée avec une erreur « stale snapshot » explicite —
  jamais résolue vers un élément voisin. Sans `snapshot`, l'uid est résolu
  contre la liste courante (l'agent qui re-snapshot régulièrement reste
  protégé par l'invalidation).
- **Surface visible uniquement** : les jobs ne s'exécutent que sur la surface
  du panneau courant, affichée (drapeau `visible` du rendu du pane). Surface
  garée (pane fermé, autre onglet) ou autre panneau → erreur explicite, jamais
  une action à l'insu de l'utilisateur.
- **Job expiré jamais exécuté** : le pont écarter au drain les jobs dont le
  caller a déjà reçu un timeout — l'exécuteur ne peut pas démarrer une action
  dont l'agent a déjà vu l'échec (pas de double exécution). Le message de
  timeout précise que la surface peut encore compléter une action déjà
  démarrée.

## 4. Ce qui reste (Phase 4)

- Annotations visuelles : l'utilisateur marque un élément dans la page, l'agent
  reçoit l'annotation + capture + contexte DOM.
- Corps de requêtes/réponses réseau dans les outils (les erreurs réseau passent
  déjà par `browser_console`/onglet Network).
