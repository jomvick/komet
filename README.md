# Komet

> Control your coding agents (Claude Code, Codex, Cursor, Devin, GitHub Copilot, Grok, Hermes, Muse Code, OpenCode, Pi, Cline) — **100% local by default**, optional multi-device sync.

Komet is a native **Rust + gpui** controller in a single binary. Each device runs a small engine that stores sessions locally. No account, no network required at install — sync only activates if you self-host it.

---

## Principles

- **Local-first** — works fully offline, data stays on device
- **Single binary** — UI + engine, headed or headless mode
- **Multiple agents** — native CLI, MSP, and ACP integrations with mid-conversation provider switching
- **Optional sync** — Loro CRDT via a self-hosted `komet-sync` server (SQLite + bearer token)

> **Security:** komet's sandbox settings constrain the agent CLI's own permissions — they are not an OS-level boundary. See [docs/security.md](docs/security.md).

---

## Installation

All binaries are available on [**GitHub Releases**](https://github.com/jomvick/komet/releases).

### Linux

**One-line install (recommended)**
```bash
curl -fsSL https://raw.githubusercontent.com/jomvick/komet/main/install.sh | sh
komet status
```
Installs `komet` to `~/.komet/app` and links it into `~/.local/bin`. To run the
engine as a user service, run `komet daemon install`.

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

## Updating

The self-updater supports Linux installs made with the one-line installer and
the macOS app bundle:

```bash
komet update
```

On Linux, run the command above. On macOS, click the update banner in Komet and
restart the app when prompted. For other installation types, download the latest
release from [GitHub Releases](https://github.com/jomvick/komet/releases) and
install or replace the existing app:

| Platform / install type | Update |
|-------------------------|--------|
| Fedora / RPM (`x86_64`) | Download the latest `komet-<version>-1.x86_64.rpm`, then run `sudo dnf install ./komet-<version>-1.x86_64.rpm` |
| Fedora / RPM (`aarch64`) | Download the latest `komet-<version>-1.aarch64.rpm`, then run `sudo dnf install ./komet-<version>-1.aarch64.rpm` |
| Debian / Ubuntu (`amd64`) | Download the latest `komet_<version>_amd64.deb`, then run `sudo apt install ./komet_<version>_amd64.deb` |
| Debian / Ubuntu (`arm64`) | Download the latest `komet_<version>_arm64.deb`, then run `sudo apt install ./komet_<version>_arm64.deb` |
| Linux AppImage | Download the latest AppImage and replace the old file |
| Linux tarball | Download and extract the latest tarball, then run its included `install.sh` |
| macOS DMG | Download the latest DMG and replace `Komet.app` in Applications |
| Windows portable EXE / ZIP | Download the latest EXE or ZIP and replace the existing `komet.exe` |

`komet update` does not update RPM/DEB packages, AppImages, or Windows portable
downloads; use the package manager or replace those files as described above.

---

## Daily usage

```bash
komet status                          # engine status + local/sync mode
komet update                          # update a Linux install made with the one-line installer
komet engine stop                     # gracefully stop the desktop app's background engine
komet daemon start|stop|restart|status
komet headless                        # engine only (no UI)
```

The desktop app starts its engine as a detached background process when one is not already
running. Closing the window or quitting Komet does not interrupt active sessions. Use
`komet engine stop` to stop that engine explicitly; `komet daemon` remains for service-managed
headless deployments.

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

### Remote devices (a VM or VPS with its own workspace)

An environment is a workspace backed by its own sync server: its sessions run on
that machine and its sidebar stays separate from your local one. Install the
agent CLIs you want to use there first (`opencode`, `hermes`, `codex`, …) — the
environment offers exactly the agents its own machine has. On the remote
machine:

```bash
curl -fsSL https://raw.githubusercontent.com/jomvick/komet/main/install.sh \
  | sh -s -- --serve --lan
```

It installs a UI-less Komet, registers it as a service, and prints a
`komet://pair/…` link. Paste that link in **Komet → Settings → Sync → Add
device**, then pick the machine from the **Devices** menu at the top of the
sidebar to chat on it and manage its agents. `--lan` serves plain HTTP, so the
dialog asks you to confirm you are on a network you control; behind TLS, use
`komet serve --public-url https://sync.example.com --install-service` instead.
The token and namespace are generated once and saved in `~/.komet/serve.json`
(0600); the link is a secret — it grants the bearer a shell on that machine.

To stop being an environment: **Settings → Sync → Remove** on the desktop (tick
*purge* to delete its local mirror too), then `systemctl --user disable --now
komet.service && rm -rf ~/.komet ~/.local/bin/komet` on the remote machine.

> Full details: [`docs/environments.md`](docs/environments.md)

---

## Architecture

```
gpui UI ── RPC (localhost / in-proc) ── engine A ══ komet-sync server ══ engine B ── RPC ── gpui UI
                          │         self-hosted, bearer-token auth         │
                          └── Loro CRDT sync (rooms + blobs) ──────────────┘
```

| Crate | Role |
|-------|------|
| `komet-engine` | sessions, agents, terminals, git/worktrees |
| `komet-ui` | gpui interface (sidebar, transcript, composer, diff) |
| `komet-doc` | Loro schemas + mirror layer |
| `komet-sync` | room client + SQLite snapshots |
| `komet-sync-server` | self-hosted sync server (rooms, blobs, bearer auth) |
| `komet-harness` | ACP adapters (7 agents) |

Learn more: [`ARCHITECTURE.md`](ARCHITECTURE.md) · [`docs/`](docs/)

---

## Development

```bash
cargo run --bin komet             # main app (UI + engine)
cargo run --bin komet-sync-server # standalone sync server
```

> `cargo run` alone fails: the workspace exposes two binaries, so `--bin` is required.

---

## Credits

Komet is a fork of [zeronsh/comet](https://github.com/zeronsh/comet) — same MIT license, same author (Wing). The fork diverges on local-first sync, self-hosted `komet-sync-server`, and gpui UI refinements. Upstream history is preserved via the `upstream` git remote (`git fetch upstream`).

## License

[MIT](LICENSE) — contributions welcome.
