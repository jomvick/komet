# Sync et migration

Guide de référence pour la synchronisation actuelle, son diagnostic et la
migration locale des conversations encore sur l'ancien format de room.

## Deux usages distincts

- **Sync des sessions et du registre** : plusieurs appareils partagent un serveur
  `komet-sync-server` et le même bearer. Sans `KOMET_EDGE_URL` et
  `KOMET_SYNC_TOKEN` non vides, le moteur reste hors ligne.
- **Environnements distants** : `komet serve` lance un moteur et un serveur
  propres à une machine distante. On l'ajoute à l'application avec le lien de
  pairing dans Settings → Sync. Voir [`environments.md`](environments.md).

Le nom `KOMET_EDGE_URL` est conservé pour compatibilité ; il désigne l'URL du
serveur self-hosted configuré par l'utilisateur. `KOMET_EDGE_TOKEN` sert
uniquement d'identité de développement et ne remplace jamais le bearer.

## Configuration self-hosted

Déployer le serveur et configurer chaque appareil comme décrit dans
[`self-hosted-sync.md`](self-hosted-sync.md). Le serveur exige un bearer partagé
et doit être exposé derrière TLS lorsqu'il est accessible depuis Internet.

Le daemon installé capture `KOMET_EDGE_URL` et `KOMET_SYNC_TOKEN` au moment de
`komet daemon install`. Après modification, réinstaller le service ou mettre à
jour son environnement, puis le redémarrer. `komet status` affiche la
configuration et l'état IPC ; il ne contacte pas le serveur.

## Diagnostic et migration s2 → chat2

Ces commandes interrogent le moteur local et requièrent Komet en cours
d'exécution :

```sh
komet sync status
komet sync status --legacy
komet sync migrate --dry-run
komet sync migrate
```

`status` donne l'état par room. `status --legacy` inventorie les chats encore
sur s2, indique s'ils ont un document local et quel appareil les héberge.
`migrate --dry-run` liste les chats qui seraient traités sans écrire. La
migration réelle est idempotente : elle doit être lancée sur l'appareil hôte de
chaque chat ; les chats hébergés ailleurs restent à migrer sur leur appareil
hôte. On peut relancer la commande après une coupure.

Ne supprimer aucun lecteur s2 pendant la période de grâce. Un chat encore
irrécupérable ou uniquement présent côté serveur doit être archivé explicitement
avant la suppression du lecteur. La période de grâce n'est pas déclarée terminée
par ce dépôt : elle dépend d'une version publiée et de l'inventaire réel des
appareils utilisateurs.

## Sauvegarde

Sauvegarder l'ensemble du répertoire de données du serveur, pas seulement les
fichiers SQLite : les rooms conservent aussi des blobs associés. Pour une copie
simple et cohérente, arrêter le serveur avant de copier son volume de données,
puis le redémarrer. Garder le bearer séparément et le protéger comme un secret.

## Import d'un projet

L'import de projet copie les fichiers d'une machine vers une autre ; il ne
migre pas l'historique des chats. Depuis la barre latérale, ouvrir le menu
Project même avec « All projects » sélectionné, puis choisir « Import project… »
sous « New project… ». Les secrets restent exclus par défaut. Le transfert est
piloté par l'application ouverte et ne reprend pas après sa fermeture.

Les détails du protocole des rooms chat2 et du registre restent dans
[`chat2-sync.md`](chat2-sync.md) et [`registry-sync.md`](registry-sync.md) ; ces
notes décrivent des choix internes et ne remplacent pas les commandes et
procédures de ce guide.
