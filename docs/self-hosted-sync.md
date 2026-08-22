# Self-hosted sync (v1)

Remplace Cloudflare Durable Objects + WorkOS + R2.

## Principe
Par défaut Komet est 100% local. Le user qui veut sync déploie lui-même `komet-sync` (VPS ou PC).

## Déploiement VPS
```bash
komet sync-init  # génère KOMET_SYNC_TOKEN
# editer .env avec le token
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
- `KOMET_EDGE_URL` : URL du sync server (défaut https://edge.komet.sh pour legacy)
- `KOMET_SYNC_TOKEN` : shared-secret Bearer (vide = open LAN)

## Stockage
- `data/rooms/*.db` : SQLite par room (frames)
- `data/blobs/` : blobs FS

## Legacy
`edge/` (Cloudflare Worker + WorkOS) est deprecated, conservé pour référence. Ne plus déployer via `wrangler`.
