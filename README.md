# Komet

> Control your coding agents (Claude Code, Codex, Cursor, Grok, Hermes, OpenCode, Pi) — **100% local by default**, optional multi-device sync.

Komet is a native **Rust + gpui** controller in a single binary. Each device runs a small engine that stores sessions locally. No account, no network required at install — sync only activates if you self-host it.

---

## Principles

- **Local-first** — works fully offline, data stays on device
- **Single binary** — UI + engine, headed or headless mode
- **Multiple agents** — unified ACP protocol (Claude, Codex, Cursor, Grok, Hermes, OpenCode, Pi)
- **Optional sync** — Loro CRDT via Cloudflare Durable Objects, self-hosted

---

## Installation

All binaries are available on [**GitHub Releases**](https://github.com/jomvick/komet/releases).

### Linux

**One-line install (recommended)**
```bash
curl -fsSL https://raw.githubusercontent.com/jomvick/komet/main/install.sh | sh
komet status
```
Installs `komet` to `~/.komet/app` and enables the user systemd service.

**Standalone AppImage**
```bash
chmod +x komet-*.AppImage
./komet-*.AppImage
```
> **Fedora / Ubuntu 24.04+:** if double-click does not work, ensure `chmod +x` is set and install FUSE (`sudo dnf install fuse` / `sudo apt install libfuse2`), or run with `--appimage-extract-and-run`.

**Tarball archive**
Download `komet-<version>-linux-<arch>.tar.gz` then run `./install.sh` inside the archive.

### macOS

| Method | File | Action |
|--------|------|--------|
| Disk Image | `komet-<version>-macos-arm64.dmg` | Drag `Komet.app` to `Applications` |
| CLI / Daemon | — | `komet daemon install && komet status` |

### Windows

| Method | File | Action |
|--------|------|--------|
| Portable | `komet-<version>-windows-x86_64.exe` / `.zip` | Extract and run `komet.exe` |
| Service | — | `komet.exe daemon install` or `komet.exe --service` |

---

## Daily usage

```bash
komet status                          # engine status + local/sync mode
komet update                          # check and install latest version
komet daemon start|stop|restart|status
komet headless                        # engine only (no UI)
```

---

## Multi-device sync (self-hosted)

Disabled by default. To sync across devices via your own server:

**1. On your VPS / server:**
```bash
komet sync-init                              # prints KOMET_SYNC_TOKEN
docker compose -f docker-compose.sync.yml up -d
# or locally: KOMET_SYNC_TOKEN=xxx komet sync-server
```

**2. On each device:**
```bash
export KOMET_EDGE_URL=http://YOUR_VPS:8787
export KOMET_SYNC_TOKEN=xxx
komet
```

> Full details: [`docs/self-hosted-sync.md`](docs/self-hosted-sync.md)

---

## Architecture

```
gpui UI ── RPC (localhost / in-proc) ── engine A ══ DeviceRoom DO ══ engine B ── RPC ── gpui UI
                          │         edge Worker (auth, rooms, R2)          │
                          └── Loro CRDT sync ── SessionRoom DO ────────────┘
```

| Crate | Role |
|-------|------|
| `komet-engine` | sessions, agents, terminals, git/worktrees |
| `komet-ui` | gpui interface (sidebar, transcript, composer, diff) |
| `komet-doc` | Loro schemas + mirror layer |
| `komet-sync` | room client + SQLite snapshots |
| `komet-harness` | ACP adapters (7 agents) |
| `edge/` | TypeScript Worker + Durable Objects + R2 |

Learn more: [`ARCHITECTURE.md`](ARCHITECTURE.md) · [`docs/`](docs/)

---

## License

[MIT](LICENSE) — contributions welcome.
