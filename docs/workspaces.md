# Workspaces, réglages par machine et import de projet

## Vocabulaire

| Terme UI | Code | Sens |
| --- | --- | --- |
| workspace | `Environment` | Un espace de travail isolé : celui de cet ordinateur (`local`) ou une machine appairée (VPS, autre poste) avec son propre serveur de sync et son propre namespace. |
| projet | `Space` | Un dossier sur UN appareil (`deviceId` + `path`). Les chemins sont toujours interprétés sur la machine propriétaire. |
| appareil | `Device` | Une machine enregistrée dans un workspace. |
| session / chat | `Chat` | Appartient au workspace qui l'héberge. Un transfert de projet ne la déplace pas. |

## Isolation

- Chaque workspace a ses propres lignes (projets, chats, sessions). `switch_workspace`
  vide toutes les listes avant d'afficher le nouveau workspace.
- La mémoire de navigation (projet, chat, appareil, filtre) est conservée **par
  workspace**, y compris pour `local` ; le filtre de projet est stocké par workspace
  (`UiSettings::space_filters`) pour qu'un filtre pointant sur un projet absent
  ailleurs ne masque pas la liste.
- L'en-tête de la sidebar affiche en permanence le libellé **et l'hôte** du workspace
  actif : deux workspaces peuvent porter le même nom, l'hôte lève l'ambiguïté.
- Les libellés d'environnements sont rendus uniques à l'appairage (« komet VPS (2) »).

## Réglages par machine

Ces réglages appartiennent à une machine et sont donc relayés par `targetDeviceId`
vers l'appareil qui possède le workspace affiché :

- extensions d'agents (`ListHarnessExtensions`, `SetHarnessExtensionEnabled`) ;
- MCP (`ListMcpServers`, …) ;
- Composio (statut, clés, comptes) ;
- comptes d'agents, usage, mises à jour, terminaux, dossiers/repos, fichiers.

`SyncStatus` reste local : il décrit les rooms du moteur auquel on est connecté.
Un affichage d'état par environnement nécessiterait d'interroger chaque appareil.

## Import de projet

« Import project… » (menu des projets) copie un dossier **d'un workspace vers un
autre** (y compris VPS → VPS). Le projet est **copié**, jamais déplacé, et ses chats
restent dans le workspace d'origine.

1. **Source** : un workspace, puis un dossier. L'aperçu (`ProjectTransferPlan`) est
   calculé **sur la machine source** : nombre de fichiers, taille, fichiers ignorés
   par `.gitignore`, fichiers ressemblant à des secrets, remote/branche/état git et
   avertissements (projet volumineux, collision de casse macOS/Windows).
2. **Destination** : dossier de base sur le workspace affiché + nom du projet.
3. **Transfert** : archive `komet archive v1` construite dans un fichier de staging
   sur la source, puis envoyée par morceaux de 1 Mio (`ProjectExportChunk` →
   `ProjectImportChunk`). Le SHA-256 est vérifié avant toute extraction.

### Sécurité

- `.gitignore` est respecté (donc `node_modules`, `target` restent derrière), et les
  fichiers de type secret (`.env*`, `*.pem`, `*.key`, `.git-credentials`, clés SSH)
  sont exclus sauf opt-in explicite.
- L'extraction refuse les chemins absolus, les `..`, et tout chemin qui traverse un
  lien symbolique ; les liens symboliques ne sont pas transportés.
- Rien n'est jamais extrait dans un dossier existant non vide : un suffixe `-2` est
  choisi à la place.
- Plafond de taille (20 Gio) et confirmation pour les projets volumineux.
- Le transfert emprunte le canal authentifié de l'environnement ; le jeton de sync
  donne déjà un accès shell sur l'hôte, le niveau de confiance est donc inchangé.

### Méthodes RPC

`ProjectTransferPlan`, `ProjectExportOpen`, `ProjectExportChunk`,
`ProjectImportBegin`, `ProjectImportChunk`, `ProjectImportCommit`,
`ProjectTransferCancel` — toutes relayables avec `targetDeviceId`.

### Limites v1

- Pas de mode « clone git » : l'archive est le seul transport pour l'instant (le
  plan renvoie `gitReady` et le remote pour préparer ce mode).
- Les chats ne suivent pas le projet.
- La progression survit à la fermeture du dialogue mais le transfert est orchestré
  par le moteur local (pas encore de reprise après redémarrage).
