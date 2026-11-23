# Plan : intégrer workspaces/import et terminer la sync sur `sync`

**Branche cible :** `sync`. Le merge de `main` est maintenant intégré dans `048090d`.

> **Plan révisé le 2026-11-22 après revue du dépôt et décision produit :** finir
> le chantier sync d'abord, rétablir l'accès à Import project dans l'UI, puis
> retirer l'ancien système d'auth/sync propre à Komet et seulement ensuite
> réorganiser les modules si les mesures montrent un gain.

**But :** intégrer les fonctions workspaces/import déjà livrées sur `main`, finir les écarts du plan workspaces, puis clôturer la migration et la simplification de sync. Le travail sera découpé en changements révisables, validés et poussés sur `sync`.

## État constaté le 2026-11-22

Le PR #86 de `main` est maintenant fusionné dans `sync` par `048090d`. Cela a également intégré les commits de configuration d’extensions qui étaient déjà sur `main`. Le code contient `project_transfer.rs`, l’écran Import project, le clonage Git, le navigateur local/distant et les indicateurs d’activité.

Sur `sync`, les phases du plan de stabilisation sont largement avancées :

- Phase 1 : route diff chat2, écritures atomiques des blobs, SQLite WAL/busy timeout, purge des rooms inactives — présentes.
- Phase 2 : scénarios de reprise e2e et matrice CI multiplateforme — présentes dans l’historique.
- Phase 3 : dial unifié, reprise au réveil/retour réseau et moniteur de réseau — présents.
- Phase 4 : transport WebSocket partagé adopté par registry, chat2 et device-room — présent.
- Phase 5 : `komet sync status --legacy` et `komet sync migrate [--dry-run]` — ajoutés. La suppression de s2 n’est pas faite : `room.rs`, `RoomClient`, le join/salvage s2 et le lecteur exemple existent toujours.
- Phase 6 : `docs/sync.md` n’existe pas ; `docs/chat2-sync.md` et `docs/registry-sync.md` sont encore les pages actives. Les permissions Windows de `environments.json` et `server-identity.json` restent à vérifier.

État workspace après intégration :

- La mémoire de navigation sauvegarde maintenant le workspace local et les workspaces appairés.
- Le filtre de projets est mémorisé par workspace dans les réglages et abandonné s’il cible un projet absent.
- Le transfert archive par morceaux, son checksum, l’extraction sécurisée, les secrets exclus par défaut, le clonage Git et le routage source/destination sont présents dans le code intégré.
- Les fonctions sont intégrées mais restent à valider par build et tests ciblés. Il faut auditer les limites d’interruption/reprise avant de promettre la reprise après fermeture de l’application.

## Décisions de départ

- Garder le comportement d’import déjà choisi sur `main` : copie du projet, sans migration de chats.
- Garder l’import Git en premier choix quand le dépôt est accessible depuis la destination. Préserver le flux de fichiers existant pour les cas sans remote exploitable.
- Exclure les secrets par défaut ; toute inclusion doit être confirmée et explicitement visible.
- Ne pas supprimer s2 avant d’avoir mesuré les chats encore migrables et laissé une période de grâce exploitable.
- Faire un merge de `main` dans `sync` pour intégrer le PR #86, plutôt qu’un cherry-pick incomplet de sa série de commits.

## Phase 0 — intégrer `main` dans `sync` — FAIT

1. Créer un point de restauration local de `sync` et confirmer l’absence de changements locaux.
2. Fusionner `main` dans `sync`, résoudre les conflits en gardant les deux ensembles de changements : transferts workspaces/import de `main` et transport/migration sync de `sync`.
3. Vérifier le diff de merge, en particulier `engine/src/rpc.rs`, `ui/src/state/workspaces.rs`, `ui/src/shell/spaces/*`, `rpc/src/lib.rs` et `docs`.
4. Vérifier que l’import distant cible le bon appareil, que les chemins sont validés sur la machine destination et qu’aucune ligne d’un autre workspace ne fuit lors d’un changement.

**Résultat :** merge `048090d` sans conflit. `cargo check --workspace --all-targets` passe.

## Phase 1 — mémoire de navigation par workspace — IMPLÉMENTÉ, À VALIDER

1. Enregistrer l’état sortant pour tout workspace, y compris `local`.
2. Ajouter `space_filter` à la mémoire de workspace et restaurer le filtre après bascule. Un filtre dont l’espace n’existe plus dans le workspace entrant doit retomber sur « Tous les projets ».
3. Tester les allers-retours local → distant → local, la restauration de l’espace/chat, le filtre absent ou obsolète et l’absence de mélange de listes.

**Code présent :** chaque environnement, y compris `local`, a sa mémoire et son filtre. Le build workspace passe ; ajouter un test de bascule ciblé si l’architecture de test GPUI le permet.

## Phase 2 — valider et durcir l’import de projet

1. Faire l’inventaire réel des capacités apportées par le merge : source/destination, Git, transfert de fichiers, aperçu, progression, exclusions et limites. Le transfert est piloté par la UI et ne survit pas encore à sa fermeture ; ne pas le présenter comme tâche de fond persistante.
2. Compléter le transfert de fichiers si nécessaire : flux en morceaux sans charger toute l’archive en mémoire, checksum de bout en bout, reprise idempotente et nettoyage des temporaires.
3. Durcir l’extraction et la création destination : rejeter chemins absolus, `..`, liens sortant de la cible et collisions de casse ; ne jamais écraser une destination existante.
4. Vérifier les exclusions `.gitignore` et secrets sur Windows/macOS/Linux, les permissions exécutables lorsque pertinentes, les chemins Unicode et les destinations avec espaces/tilde.
5. Ajouter des tests d’invariants de routage : chaque opération source passe par le device source, chaque écriture par le device destination.

**Validation effectuée :** les 18 tests de `project_transfer` passent, dont chemins malveillants, checksum/import incomplet, collisions de casse, permissions, secrets, `.gitignore` et aller-retour par chunks. Le test de garde UI n’a pas terminé dans le temps disponible ; il a été interrompu après une longue compilation silencieuse. Le build workspace complet inclut néanmoins la compilation de `komet-ui` et de ses cibles.

**Reste à faire :** vérifier sur les trois OS via CI et valider manuellement les parcours local↔distant et distant↔distant.

## Phase 3 — migration s2 sous contrôle

1. Vérifier `status --legacy` et `migrate --dry-run` sur stores vides, mixtes, hors ligne et avec documents récupérables uniquement côté serveur.
2. Confirmer l’idempotence, les journaux de migration et le comportement lorsque des chats sont irrécupérables.
3. Ajouter/actualiser les instructions d’archive côté serveur pour les rooms irrécupérables ; ne supprimer aucune donnée s2 pendant cette phase.
4. Publier une version de grâce qui garde s2 en lecture seule et fournit un moyen concret de vérifier le nombre de chats restants.
5. Après la période de grâce et une migration confirmée, retirer par lots : commande join s2, `RoomClient`/`room.rs`, salvage/fetch s2, `s2_reader`, puis les champs et branches `room_gen` devenus inutiles. Garder le stockage et les routes nécessaires à `/registry`.

**Terminé quand :** aucun chemin d’exécution s2 ne subsiste, les anciennes bases irrécupérables sont documentées/archivées, builds et tests sont verts.

## Phase 4 — documentation et sécurité Windows

1. Écrire `docs/sync.md` comme guide de référence pour le protocole actuel, les commandes `status`/`migrate`, la reprise, le déploiement self-hosted et la sauvegarde.
2. Marquer `docs/chat2-sync.md` et `docs/registry-sync.md` comme documents historiques et les relier au guide courant.
3. Vérifier les ACL effectives de `environments.json` et `server-identity.json` sous Windows ; les restreindre au compte utilisateur/service ou documenter précisément la limite si les permissions portables ne suffisent pas.
4. Garder le token en query string comme dette séparée, sauf si la migration vers l’en-tête/la première frame peut être déployée sans casser les clients existants.

## Ordre de livraison sur `sync`

1. Merge `main` → `sync` (PR #86 : workspaces et import déjà développés).
2. Fix navigation par workspace (petit changement séparé).
3. Compléments import établis par l’audit de Phase 2.
4. Validation et période de grâce de migration s2.
5. Suppression s2 uniquement après la période de grâce.
6. Docs et permissions Windows.

## Avancement de cette exécution

- [x] Merge `main` → `sync` (`048090d`).
- [x] `cargo check --workspace --all-targets`.
- [x] 18 tests moteur de transfert.
- [x] `rustfmt --edition 2024 --check crates/engine/src/project_transfer.rs`.
- [ ] CI Windows/macOS, phase de grâce et retrait s2.
- [ ] Guide unifié `docs/sync.md` et audit ACL Windows.

Chaque étape est un commit/PR relisible sur `sync`. Ne pas annoncer la suppression de s2 comme terminée tant que la phase de grâce n’est pas passée. Vérification finale avant push : `cargo fmt --all --check`, `cargo check --workspace --all-targets`, tests ciblés puis `cargo test --workspace` et clippy selon le temps de CI ; build Windows/macOS via CI.

## Fichiers repères

- Navigation : `crates/ui/src/state/workspaces.rs`, `crates/ui/src/state/engine.rs`, état de filtre dans `crates/ui/src/settings.rs` et `crates/ui/src/shell/spaces/menu.rs`.
- Import : `crates/engine/src/project_transfer.rs`, `crates/engine/src/rpc.rs`, `crates/ui/src/shell/spaces/import.rs`, `crates/rpc/src/lib.rs` (après merge de `main`).
- Sync : `crates/sync/src/{room,transport,registry,chat_client,wake,types}.rs`, `crates/sync-server/src/{lib,chat_room}.rs`, `crates/engine/src/doc_host.rs`, `apps/komet/src/main.rs`.

## Ordre révisé après revue d'architecture

### A — Clore le chantier sync sans perdre les anciennes sessions

1. Valider le contrat réel des profils et des variables de sync (`KOMET_EDGE_URL`,
   `KOMET_SYNC_TOKEN`, `KOMET_EDGE_TOKEN`) : le code active maintenant le transport
   avec URL + token sync, et conserve `KOMET_EDGE_TOKEN` comme identité dev.
2. Rendre Import project accessible depuis le sélecteur par défaut « All
   projects » ; la ligne existante après « New project… » est maintenant
   rattachée au menu de l'en-tête et s'affiche aussi dans ce mode.
3. Compléter la validation des transferts local↔VPS et VPS↔VPS, puis la CI
   multiplateforme et l'audit ACL Windows.
4. Exécuter/valider la migration s2, documenter les sessions irrécupérables et
   maintenir la période de grâce avant toute suppression de lecteur s2.
5. Unifier le guide de sync et les documents de référence après stabilisation.

**Barrière de sécurité des données :** ne pas supprimer le code s2 qui peut
encore lire ou migrer des conversations tant que la période de grâce n'est pas
terminée et que les stores connus sont migrés ou explicitement archivés.

### B — Retirer l'ancien système Komet

Après la clôture sync, retirer les flux d'authentification/organisations Komet,
les RPC et écrans de connexion sans effet, les commandes CLI devenues vides,
les variables de déploiement retirées et les références au service hébergé
précédent. Aligner README, AGENTS, ARCHITECTURE, PARITY, plans actifs, site,
scripts, exemples et tests sur le serveur self-hosted actuel.

Ne pas supprimer les formats/données de session nécessaires à la migration.
Les mots `edge` utilisés pour EdgeFade, Microsoft Edge, ou l'URL de serveur
encore configurée par `KOMET_EDGE_URL` ne sont pas à effacer mécaniquement ;
décider séparément si cette variable publique est renommée et fournir une
migration explicite si elle l'est.

### C — Séparer UI et moteur sans réarchitecture globale

Déplacer l'assemblage/démarrage du moteur hors de `komet-ui` et faire dépendre
l'interface d'un client RPC injecté. Garder le même protocole pour le mode
embarqué et le moteur distant. Le gain attendu concerne surtout les builds
ciblés de l'UI ; le build complet compile encore les deux côtés et devra être
mesuré avant/après.

### D — Réorganiser les gros modules et optimiser sur mesures

Découper par responsabilité `composer.rs`, `transcript.rs`,
`agent_accounts.rs`, `sessions.rs` et les handlers RPC. Commencer par des
modules internes, créer une crate seulement si elle isole une dépendance et
améliore un build ciblé. Traiter en priorité la reprojection O(n) du transcript
déjà identifiée dans ARCHITECTURE. Comparer compilation froide/chaude, démarrage
avec données existantes et `scripts/frame-budget.sh`; un découpage de fichiers
seul ne compte pas comme gain de performance.

### État de l'exécution après la revue

- [x] Contrat de sync/profils aligné dans le démarrage : URL + bearer non vide requis, bearer vide ignoré, token capturé par `komet daemon install` pour les services Linux, macOS et Windows. Reste à valider par une exécution à deux appareils.
- [x] Import project rattaché au sélecteur en mode « All projects » ; UI compilée, validation interactive de l'import encore à faire.
- [x] Commandes CLI `login`/`logout` sans effet supprimées ; `komet status` décrit maintenant la configuration sync et l'état IPC sans modèle de compte.
- [ ] Transferts local↔VPS et VPS↔VPS vérifiés.
- [ ] Migration s2 et période de grâce clôturées avant retrait.
- [x] Guide courant `docs/sync.md` créé ; les repères chat2/registre décrivent maintenant le protocole et son implémentation Rust actuels.
- [ ] Audit Windows des ACL pour `environments.json` et `server-identity.json` ; le contrôle runtime Windows reste nécessaire.
- [x] Ancien compte Komet retiré du code actif : CLI login/logout, écrans de connexion/organisation, RPC mutation, types utilisateur/organisation et état d’auth du shell supprimés ; seul le bearer self-hosted (`SyncStatus`) reste. Les données historiques gardent leurs chemins de profil compatibles.
- [ ] Mesures compilation/démarrage/frame capturées avant toute réorganisation.
- [ ] UI découplée d'engine et fichiers réorganisés seulement avec bénéfice mesuré.
