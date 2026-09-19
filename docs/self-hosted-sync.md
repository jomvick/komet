# Self-hosted sync (v1)

Remplace Cloudflare Durable Objects + WorkOS + R2.

## Principe
Par défaut Komet est 100% local. Le user qui veut sync déploie lui-même `komet-sync` (VPS ou PC).

## Déploiement VPS
```bash
komet sync-init  # génère KOMET_SYNC_TOKEN
# editer .env avec le token
# The container runs as uid 10001: create the data directory it writes to.
# Otherwise Docker creates it owned by root and the server cannot write.
mkdir -p ./data-sync
sudo chown 10001:10001 ./data-sync
docker compose -f docker-compose.sync.yml up -d
```

## Déploiement PC local
```bash
KOMET_SYNC_TOKEN=xxx komet sync-server --port 8787
```

## Configuration clients
Sur chaque device :
```bash
export KOMET_EDGE_URL=http://VPS_IP:8787
export KOMET_SYNC_TOKEN=xxx
komet
```

## Variables
- `KOMET_EDGE_URL` : URL du sync server (aucun défaut — non configuré = 100% local)
- `KOMET_SYNC_TOKEN` : shared bearer token. Required: the server refuses to start when it is unset or blank.
- `KOMET_SYNC_PORT` : port of the container image binary (default 8787).

## Upgrading an existing Docker deployment
The image now runs as the unprivileged user `komet` (uid 10001). Give it the existing data directory once (new deployments do this in the steps above):
```bash
sudo chown -R 10001:10001 ./data-sync
```

## Stockage
- `data/rooms/*.db` : SQLite par room (frames)
- `data/blobs/` : blobs FS

## Historique
L'ancien backend Cloudflare (Worker + Durable Objects + WorkOS + R2) a été supprimé.
