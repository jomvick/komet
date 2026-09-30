# Self-hosted sync (v1)

Remplace Cloudflare Durable Objects + WorkOS + R2.

## Principe
Par défaut Komet est 100% local. Le user qui veut sync déploie lui-même `komet-sync` (VPS ou PC).

## Déploiement VPS
```bash
komet sync-init  # génère KOMET_SYNC_TOKEN + URL https://
# editer .env avec le token
# The container runs as uid 10001: create the data directory it writes to.
# Otherwise Docker creates it owned by root and the server cannot write.
mkdir -p ./data-sync
sudo chown 10001:10001 ./data-sync
docker compose -f docker-compose.sync.yml up -d
```

## TLS via reverse proxy (requis)
Le token `KOMET_SYNC_TOKEN` est envoyé en bearer sur chaque requête : ne
l'exposez jamais en clair sur le réseau. Mettez le sync server derrière
un reverse proxy TLS.

Caddy (le plus simple) :
```
sync.example.com {
    reverse_proxy 127.0.0.1:8787
}
```

nginx :
```nginx
server {
    listen 443 ssl;
    server_name sync.example.com;
    ssl_certificate /etc/ssl/sync.example.com.crt;
    ssl_certificate_key /etc/ssl/sync.example.com.key;
    location / {
        proxy_pass http://127.0.0.1:8787;
        proxy_http_version 1.1;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection "upgrade";
    }
}
```

## Déploiement PC local
```bash
KOMET_SYNC_TOKEN=xxx komet sync-server --port 8787
```

## Configuration clients
Sur chaque device :
```bash
export KOMET_EDGE_URL=https://sync.example.com
export KOMET_SYNC_TOKEN=xxx
komet
```

Clients warn when a token is sent to an `http://` URL that is not
localhost. Trusted LAN setups without TLS can keep using plain HTTP by
opting in explicitly :
```bash
export KOMET_EDGE_URL=http://192.168.1.10:8787
export KOMET_SYNC_ALLOW_INSECURE_HTTP=1
```

## Variables
- `KOMET_EDGE_URL` : URL du sync server (aucun défaut — non configuré = 100% local)
- `KOMET_SYNC_TOKEN` : shared bearer token. Required: the server refuses to start when it is unset or blank.
- `KOMET_SYNC_PORT` : port of the container image binary (default 8787).
- `KOMET_SYNC_ALLOW_INSECURE_HTTP` : set to `1` to silence the plain-HTTP warning on a trusted LAN. Never set this for traffic over the internet.

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
