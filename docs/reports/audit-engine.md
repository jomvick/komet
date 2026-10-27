# Rapport d'audit : `komet-engine`

**Date** : 17 octobre 2026
**Périmètre** : `crates/engine/src` (22 modules + `mcp/`, 13 fichiers) et `crates/engine/tests` (22 fichiers), au commit `f418c98`
**Hors périmètre** : `komet-harness`, `komet-ui`, `komet-sync`/`sync-server`, `komet-proto`/`komet-rpc` (déjà couverts par `audit-proto-rpc.md`, `audit-sync.md`, `audit-ui.md`)

**Statut de la compilation et des tests** :

- `cargo check -p komet-engine --all-targets` : **succès**, 0 erreur, 0 avertissement (1 min 55 s, profil `dev`).
- `cargo test -p komet-engine --lib` : **172 passés**, 0 échec, 1 ignoré (4,42 s).
- `cargo test -p komet-engine --test workspace_files --test sandbox_validation --test permission_flow --test mcp_rpc` : **20 passés** (10 + 4 + 4 + 2), 0 échec.
- **Sonde E-01** : un test temporaire a appelé `Repos::delete_worktree` sur un répertoire hors dépôt ; le répertoire et son contenu ont été supprimés (sonde retirée après exécution, aucun fichier du projet touché).

> Les modifications MCP non commitées présentes dans le checkout de travail (`mcp/discovery.rs`, `mcp/status.rs`, `rpc.rs`, `tests/mcp_rpc.rs`) ont été **exclues** : l'audit porte sur `f418c98`. L'incidence est signalée en E-02.

---

## 1. Résumé exécutif

`komet-engine` est le cœur fonctionnel de Komet : 39 355 lignes de Rust, 173 tests unitaires dans `src`, 95 tests d'intégration dans 22 fichiers. L'architecture est sérieuse — journal d'exécution rejouable avec récupération de crash, registre de commandes « mark-before-execute » réellement implémenté, confinement des fichiers d'espace de travail (jail + refus des liens symboliques + refus de `.git`), écritures atomiques avec `fsync` du répertoire parent, plafond d'octets sur les captures git, verrou mono-instance `flock` documenté et testé. Seulement 56 `unwrap()/expect()` subsistent hors tests.

L'audit met néanmoins en évidence **un défaut critique destructeur** et une série de fragilités structurelles :

1. **Suppression arbitraire de répertoire (P0)** : `DeleteWorktree` accepte `worktreePath` tel quel depuis les paramètres RPC et, si `git worktree remove` échoue, exécute `std::fs::remove_dir_all(worktree_path)`. Aucune vérification que le chemin appartient au dépôt, vit sous `{worktrees_root}` ou correspond à un worktree enregistré. Un appel RPC avec `worktreePath = /home/<user>` supprime le répertoire personnel.
2. **Fonctionnalité MCP externe factice (P1)** : `McpDiscovery::list_tools` ne se connecte à rien et invente un outil `<id>_tool` ; l'endpoint répond `Ready`, et `harness_mcp_servers_for_run` — le seul lien censé transmettre les serveurs MCP assignés à l'agent — n'a **aucun appelant de production**. Le contrat RPC complet (list/get/save/delete/enable/test/list-tools/watch-status) existe donc sans effet réel.
3. **Surface RPC distante non cloisonnée (P1)** : 50+ méthodes sont `forwardable` et incluent `WriteWorkspaceFile`, `OpenTerminal`/`WriteTerminal`, `SaveMcpServer`, `ApplyUpdate` et `DeleteWorktree`. L'unique barrière est le jeton bearer partagé du relais : un pair qui le possède obtient écriture de fichiers et exécution de commandes sur l'hôte, sans scope par méthode ni vérification d'appareil.
4. **Hygiène des secrets inégale (P2)** : les valeurs MCP sont écrites en clair puis `chmod 600` après coup (fenêtre lisible), l'écriture n'est pas atomique (un crash vide le fichier, silencieusement), et le répertoire de données est créé sans permissions restrictives — journaux de conversation, `docs.sqlite` et uploads héritent de l'umask.
5. **Robustesse de l'endpoint MCP (P2)** : parseur HTTP maison à lecture unique de 8 Ko, JSON invalide converti en `Null` (réponse 200 « unknown method »), erreurs JSON-RPC renvoyées dans `result` (`_is_error` ignoré), comparaison de jeton non constante.

---

## 2. Cartographie des modules

### Table des composants

| Module | Rôle | Taille | Points sensibles |
|---|---|---|---|
| **`agent_accounts.rs`** | Détection/activation des comptes CLI (Claude, Codex, Cursor, Grok, Antigravity), lecture Keychain/Secret Service, échange de credentials, sondes d'usage. | 5 225 l. | Lecture de secrets, `lsof` synchrone, écritures de credentials. |
| **`sessions.rs`** | Moteur d'exécution : dispatch, steering, interrupt, recouvrement de session, injection MCP interne, ponts input/permission. | 2 630 l. | Cycle de vie des runs, timeouts de permission, `last_requests` en mémoire. |
| **`doc_host.rs`** | Hôte de document par chat : file de commandes durable, exécuteur hôte unique, purge, nudges de livraison. | 2 587 l. | Mark-before-execute, résolution des commandes, réseau best-effort. |
| **`workspace_files.rs`** | Lecture/écriture/recherche de fichiers d'espace de travail, watches, curseurs de répertoire. | 2 559 l. | Jail de chemins, symlinks, `.git`, écritures atomiques. |
| **`rpc.rs`** | Surface `EngineRpc` : dispatch, routage `targetDeviceId`, MCP, workspace, terminaux, uploads, updates. | 2 336 l. | `forwardable`, `DeleteWorktree`, parsing des paramètres. |
| **`diff_sync.rs`** | Captures de diff (watch, tour, commit), snapshots d'arbre, merge-base, watchers. | 1 455 l. | Appels git, index temporaire, plafonds mémoire. |
| **`lib.rs`** | Assemblage du moteur, profil d'espace, verrou, crash shield, migration legacy, serveur IPC. | 1 393 l. | Création du data dir, ordre de démarrage. |
| **`repos.rs`** | Registre de dépôts, branches, worktrees, listes de dossiers, recherche de fichiers. | 1 350 l. | Arguments git, **suppression de worktree**. |
| **`workspace_host.rs`** | Synchronisation du registre d'espaces avec le serveur (`komet-sync`), cycle de join. | 1 335 l. | Bearer, reconnexion. |
| **`registry.rs`** | Catalogue des harnais, préférences, cache de modèles, préchauffage. | 1 285 l. | Spawn de CLI, cache sur disque. |
| **`mcp/`** (13 fichiers) | Serveur MCP interne (catalogue, politique, endpoint HTTP), registre de serveurs externes, secrets, découverte. | 2 155 l. | Endpoint localhost, secrets, découverte factice. |
| **`terminals.rs`** | Sessions PTY : replay borné 1 Mo, batching 12 ms, reaper. | 423 l. | Canaux non bornés. |
| **`uploads.rs`** | Staging de pièces jointes par chunks, commit, lecture jail. | 424 l. | I/O bloqueantes, TOCTOU. |
| **`run_journal.rs`** | Journal JSONL par chat, replay, budget de reprise, détection de crash. | 418 l. | Croissance non bornée. |
| **`profile.rs`**, **`spaces.rs`**, **`local_import.rs`**, **`legacy_home.rs`** | Résolution des racines de profil, espaces, import de profil local, migration `~/.zeron` → `~/.komet`. | 1 318 l. | Permissions des racines. |
| **`auth.rs`**, **`instance_lock.rs`**, **`crash_shield.rs`**, **`titles.rs`** | Auth locale (stub), verrou mono-instance, hook de panique, titrage automatique. | 980 l. | Auth factice, `holder()` Windows. |

### Flux nominal d'un tour

```
UI ──QueueCommand(Run)──▶ doc_host (doc Loro) ──▶ drain_commands (hôte uniquement)
                                                        │  mark_processed(commande)   ← AVANT exécution
                                                        ▼
                                                 sessions.dispatch
                                                        │  validation modèle (catalogue live)
                                                        │  injection MCP interne (127.0.0.1 éphémère)
                                                        ▼
                                              harness (CLI agent, sous-processus)
                                                        │  événements + journal JSONL
                                                        ▼
                                        doc (transcript) ── WATCH_DOC_MESSAGES ──▶ UI
```

Le moteur est **hôte unique par chat** (`is_host`), ce qui rend la déduplication des commandes correcte : une seule instance écrit le résultat. Les journaux servent à la fois de source de replay et de jauge de crash (dernier événement ≠ `Done` ⇒ run mort).

---

## 3. Tableau des constats

| ID | Priorité | Fichier:Ligne | Preuve de code | Impact | Correctif proposé | Effort |
|---|---|---|---|---|---|---|
| **E-01** | **P0** | `rpc.rs:1965-1974` + `repos.rs:749-771` | `delete_worktree(Path::new(&p.repo_path), Path::new(&p.worktree_path))` → si `git worktree remove --force` échoue : `let _ = std::fs::remove_dir_all(worktree_path);` | [Vérifié **empiriquement**] Suppression récursive d'un répertoire arbitraire choisi par l'appelant RPC (local ou relayé). Aucun contrôle d'appartenance au dépôt, ni de racine, ni de présence dans `git worktree list`. | Valider que `worktree_path` est canonique, sous `worktrees_root`, et présent dans `git worktree list --porcelain` du dépôt ; sinon `BadParams`. Ne **jamais** retomber sur `remove_dir_all` sans ces vérifications. | S |
| **E-02** | **P1** | `mcp/discovery.rs:107-177` + `rpc.rs:960-975` | `Self::fake_tools_list(config)` ; commentaire « In real impl, this would call rmcp client's tools/list » ; retour `vec![DiscoveredTool { name: format!("{}_tool", config.id), … }]` puis `"status": McpStatus::Ready` | [Vérifié] `TestMcpServer`/`ListMcpTools`/`WatchMcpStatus` annoncent un serveur sain et un outil inexistant sans aucune connexion. Un utilisateur valide une configuration morte. (Correctif déjà présent **non commité** dans le checkout : `Unverified` + liste vide.) | Committer le correctif en cours ; supprimer `fake_tools_list` ; à défaut, retourner `McpStatus::Unverified` + `tools: []` et un message explicite. | S |
| **E-03** | **P1** | `sessions.rs:309-318` | `pub fn harness_mcp_servers_for_run(...)` — commentaire « Task 4/5 wires this into acp/claude/codex via into_harness_config » ; **aucun appelant** hors tests (`grep` sur tout le dépôt) ; `crates/harness` ne connaît pas MCP | [Vérifié] Les serveurs MCP assignés à un chat (`ChatConfig.mcp_server_ids`) sont résolus puis jetés : le run les ignore. La fonctionnalité « MCP externe » est un contrat RPC sans effet. | Soit brancher `into_harness_config` dans le lancement de chaque harnais, soit retirer la surface RPC et l'entrée de config tant que ce n'est pas fait (pas de demi-fonctionnalité exposée). | M |
| **E-04** | **P1** | `rpc.rs:1020-1093` | `forwardable()` inclut `WRITE_WORKSPACE_FILE`, `OPEN_TERMINAL`, `WRITE_TERMINAL`, `SAVE_MCP_SERVER`, `DELETE_WORKTREE`, `APPLY_UPDATE` ; l'autorisation est le bearer du relais (`Auth` → `TokenSource`), sans contrôle par méthode ni par appareil | [Vérifié] Un pair détenant `KOMET_SYNC_TOKEN` peut écrire des fichiers, ouvrir un shell et supprimer des worktrees sur l'hôte distant. Aucun journal d'audit de ces décisions (le plan `sandbox-access-improvement-plan` §D5 reste non implémenté). | Scinder la liste en capacités (`read`, `write`, `exec`, `admin`) et exiger une capacité par méthode ; journaliser les appels mutants ; à terme, jeton par appareil/room. | M |
| **E-05** | **P2** | `mcp/server.rs:281, 380-392` | `let (result, _is_error) = match method { … }` puis `json!({"jsonrpc":"2.0","id": id, "result": result})` | [Vérifié] Les refus de politique et erreurs d'exécution d'outil sont renvoyés comme **résultats** JSON-RPC valides (`isError` est une extension MCP, pas la sémantique JSON-RPC). Un client strict lit un succès. | Émettre `{"error": {"code": -32000, "message": …}}` quand `is_error`, et `-32601` pour méthode inconnue. | S |
| **E-06** | **P2** | `mcp/server.rs:186-273` | `let n = stream.read(&mut buf).await?` sur un buffer de 8 192 octets, puis parsing ligne à ligne ; `content_length` borné à `MAX_REQUEST_BYTES` ; corps complété par **une seule** lecture de 5 s ; `serde_json::from_str(&body_str).unwrap_or(Null)` | [Vérifié] Un en-tête > 8 Ko ou un corps fragmenté en plusieurs segments TCP produit un corps tronqué ou un `Null` silencieux → `200 {"error":"unknown method"}`. Pas de timeout sur la première lecture (slowloris local). | Remplacer par `hyper`/`axum` (déjà dans l'arbre via `sync-server`) ou implémenter une boucle de lecture jusqu'à `\r\n\r\n` + `content_length`, avec timeout global et réponse 400 sur JSON invalide. | M |
| **E-07** | **P2** | `mcp/server.rs:246-254` | `let h: [u8; 32] = Sha256::digest(token.as_bytes()).into(); h == auth.token_hash` | [Vérifié] Comparaison de tableaux non constante (early-exit memcmp) là où `komet-rpc` prend soin d'utiliser une comparaison à temps constant (`ipc_token.rs`). Le secret comparé est un hash, donc l'exploitabilité est faible, mais l'incohérence est réelle. | Utiliser `subtle::ConstantTimeEq` (ou la primitive exposée par `komet-rpc`) sur les deux empreintes. | S |
| **E-08** | **P2** | `mcp/secrets.rs:119-131` | `std::fs::write(&self.path, data)?;` **puis** `perms.set_mode(0o600); set_permissions(...)` | [Vérifié] Le fichier est créé avec les droits de l'umask (souvent 0644) avant d'être restreint : fenêtre de lecture pour un autre compte local. L'écriture n'est pas atomique : un crash laisse un JSON tronqué, et `load()` (`secrets.rs:106-117`) ignore l'erreur → **tous les secrets MCP sont perdus en silence**. | `OpenOptions::new().mode(0o600).create_new/new` sur un fichier temporaire puis `rename` (comme `agent_accounts::write_file_atomic`), et remonter l'erreur de parsing de `load()`. | S |
| **E-09** | **P2** | `lib.rs:181, 200-201` ; `profile.rs:153` | `std::fs::create_dir_all(data_dir)?;` — aucun `set_permissions` sur le data dir ni sur `store_root()`/`journals/` | [Vérifié] Avec un umask 022, `~/.komet` (ou `KOMET_DATA_DIR`) est en 0755 et les journaux `*.jsonl`, `docs.sqlite` et `uploads/` en 0644 : tout compte local lit les transcripts complets (prompts, code, sorties d'outils). Seuls les fichiers explicitement `secret` sont en 0600. | Créer le data dir et ses sous-racines en 0700 (`DirBuilder::mode`) et resserrer les fichiers en 0600 à la volée. | S |
| **E-10** | **P2** | `run_journal.rs:107-118, 149-176, 254-268` | `OPEN_FILE_CAP = 16` → `files.clear()` ; `replay()` et `scan_tail()` font `read_lines(path)` sur **tout** le fichier ; aucune compaction (« Bounded-window compaction is deferred ») | [Vérifié] Chaque réouverture (fréquente : cap 16 atteint, ou après un `discard`) relit et parse l'intégralité du journal ; `replay()` matérialise tout en mémoire. Un chat long produit un fichier de plusieurs centaines de Mo et un pic mémoire à chaque `Subscribe`. | Implémenter la compaction prévue (fenêtre + `Done` conservé) et un index de seq (offset) pour éviter le scan complet ; à défaut, documenter une taille maximale. | M |
| **E-11** | **P2** | `terminals.rs:211, 243` | `mpsc::unbounded_channel::<Vec<u8>>()` (sortie PTY brute) et `mpsc::unbounded_channel()` (abonnés) | [Vérifié] Le replay est borné à 1 Mo, mais un abonné qui ne consomme pas accumule indéfiniment ; idem pour la sortie brute si le batcher est en retard. Un `yes` dans un terminal sans lecteur croît sans limite. | Canaux bornés (`mpsc::channel(N)`) + politique de drop/fermeture explicite pour l'abonné lent. | S |
| **E-12** | **P2** | `agent_accounts.rs:3417-3436` | `std::process::Command::new("lsof").args(...).output()` — API **synchrone**, sans timeout, sur le chemin de détection de compte | [Vérifié] Bloque un worker tokio (runtime multi-thread partagé) et peut pendre indéfiniment sur un montage réseau ou un `lsof` verrouillé. Le reste du module utilise `tokio::process` + `timeout`. | Passer à `tokio::process::Command` avec un `timeout` de 2-5 s, et traiter l'expiration comme « pas de port ». | S |
| **E-13** | **P2** | `repos.rs:300-320`, `repos.rs:573-600` ; `diff_sync.rs:1216` | `self.git(&["clone", trimmed, &target…])` où `trimmed` vient de l'URL RPC ; `self.git(&["checkout", ref_name])` ; `merge_base(root, base_ref)` → `git merge-base <base_ref> HEAD` | [Vérifié] Injection d'arguments git : une « URL » `--upload-pack=<cmd>` est interprétée comme option (`git clone` exécute alors la commande), de même qu'une `ref_name`/`base_ref` commençant par `-`. Le nom de cible dérivé de l'URL n'est pas assaini : sur Windows, `https://x/..\..\evil` produit `repos_dir.join("..\\..\\evil")` (traversée). | Rejeter toute valeur commençant par `-`, utiliser `--` devant les refs (`checkout -- <ref>` n'est pas valide pour une ref, mais `git switch --detach -- <ref>`/`show-ref --verify` le sont), valider les refs avec `git check-ref-format`, et réutiliser `sanitize()` (déjà utilisé par `create`) pour le nom de clone. | S |
| **E-14** | **P3** | `agent_accounts.rs:4275-4296` | `let tmp = file.with_extension(format!("tmp-{}", std::process::id()));` … `std::fs::rename(&tmp, file)` | [Vérifié] Nom temporaire non unique (deux écritures concurrentes du même fichier dans un processus se marchent dessus), pas de `fsync` avant rename ; sur Windows, `secret` est ignoré (`let _ = secret;`) donc aucune DACL — même classe que **C-09** du rapport `audit-proto-rpc.md`. | Utiliser un nom aléatoire (`tempfile::NamedTempFile` dans le même répertoire), `sync_all()` avant rename, et une ACL restreinte sous Windows. | M |
| **E-15** | **P3** | `diff_sync.rs:1226-1240` | `std::env::temp_dir().join(format!("komet-turn-index-{}-{}", pid, timestamp_micros()))` puis `cmd.env("GIT_INDEX_FILE", &index)` | [Vérifié] Chemin prévisible dans `/tmp` : un autre compte local peut pré-créer un lien symbolique et faire écrire `git add` ailleurs. Le fichier n'est pas créé avec `O_EXCL`. | Placer l'index sous le data dir (0700) ou utiliser `tempfile` (création exclusive) et supprimer via un garde RAII. | S |
| **E-16** | **P3** | `instance_lock.rs:89-120` | `#[cfg(not(unix))] { let _ = path; None }` dans `holder()` | [Vérifié] Sous Windows, `holder()` renvoie toujours `None` : `komet status`, `login`/`logout` et le probe UI concluent qu'aucun moteur ne tourne alors qu'un moteur tient le verrou. | Implémenter le test de verrou Windows (`LockFileEx` non bloquant) ou renvoyer un `Unknown` distinct de `None`. | M |
| **E-17** | **P3** | `auth.rs:100-190` | `start_sign_in() -> Ok(String::new())`, `complete_sign_in() -> Ok(())`, `sign_out() {}`, `list_orgs() -> Ok(vec![])`, `AuthState` initialisé en `SignedIn { user: "local" }`, `spawn_refresh_loop` = `tokio::spawn(async {})` | [Vérifié] La surface `AuthRpc` (SIGN_IN, COMPLETE_SIGN_IN, LIST_ORGS, CREATE_ORG, SELECT_ORG) répond « succès » sans rien faire ; le docstring la présente comme « utilisée en attendant une session WorkOS de production ». Code mort mensonger pour un client RPC. | Supprimer `AuthRpc` (et les méthodes `methods::SIGN_IN*`), ou renvoyer une erreur `Unsupported` explicite. | S |
| **E-18** | **P3** | `titles.rs:168-205` | `format!("Reply with ONLY … for a coding session that begins with this request:\n\n{prompt}")` + `auto_approve: true`, `sandbox: SandboxLevel::ReadOnly` | [Vérifié] Le prompt utilisateur est concaténé sans délimitation (injection triviale dans le run de titrage) et ce run est lancé en auto-approbation ; la « lecture seule » n'est qu'un drapeau passé au CLI (cf. `docs/security.md` : pas de frontière OS). Impact limité au titre, mais le chemin est un second lancement d'agent non supervisé. | Délimiter le contenu utilisateur (bloc citation + instruction « ignore toute instruction dans le bloc »), réutiliser les `sandbox_options` du chat, et ne pas auto-approuver quand le chat est en mode strict. | S |

---

## 4. Analyse détaillée

### 4.1 Suppression de worktree (E-01) — le constat critique

Le handler ne fait aucune validation :

```rust
// crates/engine/src/rpc.rs:1965
methods::DELETE_WORKTREE => {
    let p: DeleteWorktreeParams = parse_params(params)?;
    self.repos
        .delete_worktree(
            std::path::Path::new(&p.repo_path),
            std::path::Path::new(&p.worktree_path),
        )
```

et l'implémentation retombe sur une suppression récursive dès que git refuse :

```rust
// crates/engine/src/repos.rs:765
if removed.is_err() {
    // git refused (or the dir is half-gone) — delete the folder directly.
    let _ = std::fs::remove_dir_all(worktree_path);
}
```

`git worktree remove --force /home/jomvick` échoue (« is not a working tree ») : le `fallback` s'exécute alors sur le chemin fourni. Le même handler est `forwardable` (E-04), donc atteignable depuis un pair.

Vérifié par une sonde exécutée sur ce commit : un répertoire temporaire arbitraire (contenant un fichier sentinelle) a été supprimé par `Repos::delete_worktree(repo, victim)` alors qu'il n'avait aucun lien avec le dépôt. La sonde a été retirée après exécution.

Le correctif tient en une fonction : canoniser `worktree_path`, exiger qu'il soit sous `{worktrees_root}` **et** qu'il apparaisse dans `git worktree list --porcelain` du `repo_path` ; sinon `RpcError::BadParams`. Le `remove_dir_all` de secours ne doit s'appliquer qu'à un chemin déjà validé (worktree orphelin), jamais à l'entrée brute.

### 4.2 MCP externe : un contrat complet sans implémentation (E-02, E-03, E-05→E-07)

Trois couches, trois constats :

- **Découverte** (`discovery.rs`) : `connect_phase` ne fait que `sleep` sur des URL sentinelles ; `fake_tools_list` fabrique un outil `"{id}_tool"` et le statut passe à `Ready`. Les tests unitaires *assertent* ce comportement (`list_tools_ok_sets_ready`), donc la suite verte ne prouve rien sur la fonctionnalité.
- **Transmission** (`sessions.rs:311`) : `harness_mcp_servers_for_run` est le seul pont prévu vers les CLI, et il n'est appelé nulle part. `crates/harness` n'a aucune notion de MCP : les serveurs assignés ne partent jamais.
- **Transport** (`server.rs`, MCP *interne* cette fois) : endpoint localhost correctement protégé par bearer + `callerAgentId`, mais parseur HTTP fragile et sémantique d'erreur JSON-RPC incorrecte.

À noter que l'endpoint **interne** (outils `komet` exposés à l'agent) est, lui, réel : catalogue, politique `allow/ask/deny`, timeout de permission (60 s, Deny par défaut), sémaphore 4 appels, timeout d'outil 30 s. C'est la partie externe qui est fictive.

### 4.3 Surface RPC et modèle d'autorisation (E-04)

`forwardable()` est une liste blanche **de méthodes**, pas de capacités. Elle mélange aujourd'hui : lecture (`ListRepos`, `ListModels`), mutation de workspace (`WriteWorkspaceFile`), exécution (`OpenTerminal`, `WriteTerminal`), administration (`SaveMcpServer`, `ApplyUpdate`) et destruction (`DeleteWorktree`). Le seul contrôle est le bearer du relais, partagé par tous les appareils, et la cible se contente de comparer `targetDeviceId` au sien pour éviter les boucles.

Conséquence : la frontière de sécurité réelle entre deux appareils de l'utilisateur est un secret partagé unique. Le commentaire de `docs/security.md` (« ce n'est pas une frontière noyau ») s'applique aussi ici, un cran plus haut.

### 4.4 Secrets et permissions de fichiers (E-08, E-09, E-14)

Le module est paradoxal : `McpServerConfig` masque ses valeurs dans `Debug`, `public_view()` ne renvoie que `has_secrets`, et le store est « write-only » côté RPC — bonnes décisions. Mais :

- `McpSecretStore::save` écrit puis chmod (fenêtre), n'est pas atomique (perte silencieuse), et `default_path()` retombe sur `/tmp/komet-mcp-secrets.json` si `home_dir()` est indisponible.
- Le data dir n'est jamais restreint : les journaux (contenu intégral des conversations), `docs.sqlite`, les uploads et `repos/` sont en 0644/0755 selon l'umask.
- `write_file_atomic` (credentials CLI) est atomique mais sans nom unique ni `fsync`, et sans équivalent DACL sous Windows — le même angle mort que C-09 pour le jeton IPC.

### 4.5 Durabilité, mémoire, I/O (E-10→E-12)

Points solides à souligner avant les réserves : le journal tolère une ligne tronquée, reprend sa numérotation après réouverture, et le budget de reprise (`MAX_AUTO_RESUME` persisté) empêche une boucle crash→relance. `capture_git` tue l'enfant au plafond d'octets. Les écritures de fichiers d'espace passent par un temporaire + `sync_parent_directory`.

Réserves : le journal n'est jamais compacté et est relu intégralement à chaque réouverture ; les terminaux utilisent des canaux non bornés malgré un replay borné ; `lsof` est lancé en synchrone sans timeout. Aucun de ces points n'est un bug fonctionnel immédiat, mais ils se cumulent sur des sessions longues — précisément le cas d'usage de Komet.

### 4.6 Ce qui est réellement bien fait

- `drain_commands` : `mark_processed` **avant** `execute`, arrêt du drain si le registre échoue, écriture du résultat par le seul hôte. C'est la bonne sémantique, testée.
- `workspace_files` : rejet de `..`, des chemins absolus, de `\`, de `:`, des composants `.git`, refus des liens symboliques composant par composant, `canonicalize` + `starts_with(root)`, écriture atomique, `fsync` du répertoire parent.
- `instance_lock` : `flock` non bloquant avec budget de reprise documenté (fenêtre fork→exec), message d'erreur portant le PID détenteur, libération noyau en cas de crash.
- `run_journal` : reprise de seq après réouverture, tolérance aux lignes tordues, détection des runs sans `Done`.
- `capture_git` : plafond d'octets avec `start_kill`, pas de buffer illimité.
- 56 `unwrap/expect` seulement hors tests, et quasi tous sur des invariants locaux (`format!`/sérialisation d'un type interne).

---

## 5. Hypothèses auditées

| # | Hypothèse | Verdict | Preuve |
|---|---|---|---|
| H1 | Le moteur applique lui-même le bac à sable | **Infirmé** | `validate_run_request` valide puis transmet ; l'application est celle du CLI (cf. `docs/security.md`). |
| H2 | Un pair ne peut pas écrire de fichiers sur cet appareil | **Infirmé** | `forwardable()` inclut `WRITE_WORKSPACE_FILE`, `OPEN_TERMINAL`, `SAVE_MCP_SERVER`. |
| H3 | Les secrets MCP ne sortent jamais par RPC | **Confirmé** | `public_view()` / `has_secrets` ; store write-only ; aucune valeur dans les réponses. |
| H4 | Une commande n'est jamais exécutée deux fois après un crash | **Confirmé** | `mark_processed` avant `execute` + registre SQLite + hôte unique. |
| H5 | Les accès fichiers d'espace sont confinés | **Confirmé** | Jail complète, symlinks refusés, `.git` refusé, canonicalisation. |
| H6 | Les terminaux sont bornés en mémoire | **Partiellement** | Replay 1 Mo, `MAX_TERMINALS` 32, mais canaux d'abonnés non bornés. |
| H7 | Le MCP externe fonctionne | **Infirmé** | Découverte factice + aucun câblage vers le harnais. |
| H8 | Le moteur est mono-instance | **Confirmé sur Unix, absent sur Windows** | `flock` + tests ; `holder()` no-op Windows. |
| H9 | Les captures de diff ne peuvent pas saturer la mémoire | **Confirmé** | `capture_git` avec plafond et kill. |

---

## 6. Plan d'action ordonné et testable

### Tâche 1 — Neutraliser la suppression arbitraire de worktree (E-01, P0)
Valider `worktree_path` (canonique, sous `worktrees_root`, présent dans `git worktree list --porcelain`) avant tout `remove_dir_all`.
**Test** : `DeleteWorktree { worktreePath: "/tmp/x" }` → `BadParams` ; `/tmp/x` intact. Cas nominal (worktree orphelin) toujours supprimé.

### Tâche 2 — Découper la surface relayée en capacités (E-04, P1)
Table `method → capacité`, refus explicite, journalisation des appels mutants.
**Test** : un relais avec capacité `read` reçoit `Forbidden` sur `WriteWorkspaceFile`, `OpenTerminal`, `DeleteWorktree`.

### Tâche 3 — Trancher le MCP externe (E-02, E-03, P1)
Committer le correctif `Unverified` déjà écrit, puis soit câbler `into_harness_config` dans chaque harnais, soit retirer la surface RPC et `mcp_server_ids` de `ChatConfig`.
**Test** : `ListMcpTools` sur une configuration valide → `Unverified`, `tools: []` ; un chat avec `mcp_server_ids` non vide ne doit pas produire un run silencieusement sans MCP.

### Tâche 4 — Durcir l'endpoint MCP (E-05, E-06, E-07, P2)
Passer à `axum` (déjà utilisé par `sync-server`) ou corriger lecture/timeout ; erreurs JSON-RPC dans `error` ; comparaison constante.
**Test** : JSON invalide → 400 ; méthode inconnue → `-32601` ; outil refusé par politique → `error` ; en-tête de 16 Ko → traité correctement.

### Tâche 5 — Hygiène des secrets et du data dir (E-08, E-09, E-14, P2)
Data dir et sous-racines en 0700 ; `mcp-secrets.json` créé en 0600 via `NamedTempFile` + `fsync` + rename ; `load()` remonte les erreurs ; noms temporaires uniques.
**Test** : après création, `stat -c %a` sur le data dir → `700`, sur le fichier de secrets → `600` ; un JSON corrompu provoque une erreur visible au démarrage, pas un store vide.

### Tâche 6 — Bornes mémoire et I/O (E-10, E-11, E-12, P2)
Canaux bornés pour les terminaux ; compaction/index du journal ; `lsof` en `tokio` + timeout.
**Test** : un abonné qui ne lit pas ne fait pas croître la RSS au-delà du seuil ; `Subscribe` sur un journal de 100 Mo ne lit pas tout le fichier.

### Tâche 7 — Injection d'arguments git (E-13, P2)
Rejet des valeurs commençant par `-`, `check-ref-format` pour les refs, assainissement du nom de clone (réutiliser `sanitize`).
**Test** : `CloneRepo { url: "--upload-pack=touch /tmp/pwn" }` → `BadParams`, aucun fichier créé ; `SwitchRef { ref: "-b" }` → `BadParams`.

### Tâche 8 — Nettoyage de la surface d'auth (E-17, E-16, E-18, P3)
Supprimer `AuthRpc`/`methods::SIGN_IN*` ou renvoyer `Unsupported` ; implémenter `holder()` Windows ; délimiter le prompt de titrage et réutiliser les options de sandbox du chat.
**Test** : `SIGN_IN` → `UnknownMethod`/`Unsupported` ; `komet status` sous Windows rapporte le détenteur du verrou.

---

## 7. Limites de l'audit

- **Statique + compilation** : `cargo check --all-targets` et les tests ciblés ont été exécutés ; les tests d'intégration complets (`e2e`, `m5_*`, `workspace_sync`, `restart_resume`) ne l'ont pas été (coût de compilation et durée sur la machine d'audit). Aucune conclusion ne dépend de leur résultat.
- **Windows/macOS non exécutés** : les constats E-14, E-16 et la traversée de E-13 sont déduits du code (`cfg`, `std::path`), pas mesurés.
- **Modifications non commitées exclues** : le checkout de travail contient un correctif MCP (`discovery.rs`, `rpc.rs`, `tests/mcp_rpc.rs`) postérieur à `f418c98`. E-02 en tient compte mais le rapport décrit l'état commité.
- **Multicompte local non testé** : E-08/E-09 supposent un umask standard (022) et un autre compte local ; non reproduit, car cela exige deux utilisateurs sur la machine.
- **`komet-harness` et `komet-ui` hors périmètre** : l'absence de MCP côté harnais est constatée par `grep` (aucune mention), pas par lecture exhaustive de 28 500 lignes.
- **Aucun test d'intrusion réel** n'a été mené sur le relais : E-04 est un constat d'architecture (liste de méthodes + modèle de jeton), pas une exploitation.

---

## 8. Suivi des correctifs (appliqués après l'audit)

Les correctifs ci-dessous sont appliqués dans l'arbre de travail (non commités) et vérifiés par la suite de tests du crate.

| ID | État | Correctif | Test de non-régression |
|---|---|---|---|
| **E-01** | **Corrigé** | `Repos::authorize_worktree_delete` : canonicalisation, appartenance au `worktrees_root`, présence dans `git worktree list --porcelain` ; le `remove_dir_all` de secours ne reçoit plus que ce chemin validé. | `repos::tests::delete_worktree_refuses_a_directory_outside_the_worktrees_root`, `…_refuses_an_unregistered_directory_under_the_root` |
| **E-13** | **Corrigé** | `reject_option_like` sur l'URL de clone, le ref de `switch_ref`, la branche de `create_worktree` et le `base_ref` de `merge_base` ; `sanitize_repo_name` réutilisé par `clone_repo` et `create`. | `repos::tests::repo_names_are_single_safe_components`, `…::rpc_supplied_arguments_cannot_become_git_flags`, `…::clone_repo_rejects_option_shaped_urls_before_spawning_git` |
| **E-08** | **Corrigé** | `McpSecretStore::save` : fichier temporaire créé en `0600` dès l'ouverture, `sync_all`, puis `rename` ; répertoire parent en `0700` ; un chargement en échec est journalisé au lieu d'être avalé. | `mcp::tests_config` (mode `0600` après écriture) |
| **E-09** | **Corrigé** | `ensure_private_dir` (0700, création et resserrement) appliqué au data dir, aux racines de profil, au répertoire des journaux et au parent du store MCP. | `private_dir_tests::creates_and_tightens_a_private_directory` |
| **E-05** | **Corrigé** | Les échecs MCP sortent en `error` JSON-RPC (`-32000` pour un outil refusé/en erreur, `-32601` pour une méthode inconnue) au lieu de `result`. | `mcp::server::tests::unknown_method_returns_a_jsonrpc_error` |
| **E-07** | **Corrigé** | `constant_time_eq` (XOR accumulé + `black_box`) sur les empreintes SHA-256 du jeton. | couvert par les tests d'authentification de l'endpoint |
| **E-11** | **Corrigé** | Canaux bornés : file d'abonné `256` (+ la taille du replay) avec déconnexion de l'abonné qui ne draine plus, et file PTY `256` avec `blocking_send` (contre-pression vers le processus). | `m5_repos_diffs_terminals` (replay + tail + reconnexion) |
| **E-12** | **Corrigé** | `lsof` borné à 2 s (spawn + `try_wait` + `kill`) et `discover_language_servers` déplacé sur `spawn_blocking`. | compilation + chemin de listing des comptes |
| **E-06** | Partiel | Parseur HTTP MCP inchangé ; seule la sémantique d'erreur a été corrigée. | — |
| E-02, E-03 | Ouverts | Décision produit requise (committer le correctif `Unverified` déjà écrit puis câbler le harnais, ou retirer la surface). | — |
| E-04, E-10, E-14→E-18 | Ouverts | Capacités de relais, compaction du journal, P3. | — |

**Vérifications après correctifs** :

- `cargo check -p komet-engine --all-targets` : succès, 0 erreur, 0 avertissement.
- `cargo test -p komet-engine --lib` : **179 passés**, 0 échec, 1 ignoré (4,03 s).
- `cargo test -p komet-engine --test m5_repos_diffs_terminals --test workspace_files --test permission_flow --test mcp_rpc --test sandbox_validation` : 20 + 2 + 4 + 10 + 4 passés.
  - Note : lancé avec les cinq binaires en parallèle sur une machine à 4 cœurs, `permission_timeout_auto_denies_and_resolves` a échoué une fois (10 s au lieu de 0,6 s) ; relancé seul puis en suite complète, il passe. C'est de la sensibilité à la charge, pas une régression des correctifs.
- Le crate `komet-ui` n'a pas été recompilé (coût gpui) ; aucun de ses appels n'utilise les signatures modifiées (vérifié : `Terminals::subscribe` n'est appelé que depuis `engine/src/rpc.rs` et les tests du crate).
