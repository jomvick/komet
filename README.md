# Komet

> Contrôlez vos agents de code (Claude Code, Codex, Cursor, Grok, Hermes, OpenCode, Pi) — **100% local par défaut**, synchro multi-device en option.

Komet est un contrôleur natif **Rust + gpui** en un seul binaire. Chaque appareil fait tourner un petit moteur qui stocke les sessions localement. Aucun compte, aucun réseau requis à l'installation — la synchronisation s'active uniquement si vous l'hébergez vous-même.

---

## Principes

- **Local-first** — tout fonctionne hors-ligne, données sur l'appareil
- **Un seul binaire** — interface + moteur, mode headed ou headless
- **Agents multiples** — protocole ACP unifié (Claude, Codex, Cursor, Grok, Hermes, OpenCode, Pi)
- **Sync optionnelle** — CRDT Loro via Cloudflare Durable Objects, auto-hébergée

---

## Installation

Tous les binaires sont sur la page [**GitHub Releases**](https://github.com/jomvick/komet/releases).

### Linux

**Installation en une ligne (recommandé)**
```bash
curl -fsSL https://raw.githubusercontent.com/jomvick/komet/main/install.sh | sh
komet status
```
Installe `komet` dans `~/.komet/app` et active le service systemd utilisateur.

**AppImage autonome**
```bash
chmod +x komet-*.AppImage
./komet-*.AppImage
```
> **Fedora / Ubuntu 24.04+ :** si le double-clic ne fonctionne pas, vérifiez `chmod +x` et installez FUSE (`sudo dnf install fuse` / `sudo apt install libfuse2`), ou lancez avec `--appimage-extract-and-run`.

**Archive tarball**
Téléchargez `komet-<version>-linux-<arch>.tar.gz` puis exécutez `./install.sh` dans l'archive.

### macOS

| Méthode | Fichier | Action |
|---------|---------|--------|
| Disk Image | `komet-<version>-macos-arm64.dmg` | Glisser `Komet.app` vers `Applications` |
| CLI / Daemon | — | `komet daemon install && komet status` |

### Windows

| Méthode | Fichier | Action |
|---------|---------|--------|
| Portable | `komet-<version>-windows-x86_64.exe` / `.zip` | Extraire et lancer `komet.exe` |
| Service | — | `komet.exe daemon install` ou `komet.exe --service` |

---

## Usage quotidien

```bash
komet status                          # état moteur + mode local/synchro
komet update                          # vérifier et installer la dernière version
komet daemon start|stop|restart|status
komet headless                        # moteur seul (sans UI)
```

---

## Synchro multi-device (auto-hébergée)

Désactivée par défaut. Pour synchroniser plusieurs appareils via votre propre serveur :

**1. Sur votre VPS / serveur :**
```bash
komet sync-init                              # affiche KOMET_SYNC_TOKEN
docker compose -f docker-compose.sync.yml up -d
# ou en local : KOMET_SYNC_TOKEN=xxx komet sync-server
```

**2. Sur chaque appareil :**
```bash
export KOMET_EDGE_URL=http://VOTRE_VPS:8787
export KOMET_SYNC_TOKEN=xxx
komet
```

> Détails complets : [`docs/self-hosted-sync.md`](docs/self-hosted-sync.md)

---

## Architecture

```
gpui UI ── RPC (localhost / in-proc) ── engine A ══ DeviceRoom DO ══ engine B ── RPC ── gpui UI
                          │         edge Worker (auth, rooms, R2)          │
                          └── Loro CRDT sync ── SessionRoom DO ────────────┘
```

| Crate | Rôle |
|-------|------|
| `komet-engine` | sessions, agents, terminaux, git/worktrees |
| `komet-ui` | interface gpui (sidebar, transcript, composer, diff) |
| `komet-doc` | schémas Loro + couche mirror |
| `komet-sync` | client rooms + snapshots SQLite |
| `komet-harness` | adapteurs ACP (7 agents) |
| `edge/` | Worker TypeScript + Durable Objects + R2 |

En savoir plus : [`ARCHITECTURE.md`](ARCHITECTURE.md) · [`docs/`](docs/)

---

## Licence

[MIT](LICENSE) — contributions bienvenues.
