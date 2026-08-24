# Komet

Control your coding agents (Claude Code, Codex, Cursor, Grok, Hermes, OpenCode, Pi) locally by default, with optional multi-device sync.

Every device runs a small engine that stores sessions on that device. A new installation starts in local-only mode without an account or a network connection.

---

## 📥 Downloads & Installation

All pre-built binaries and packages are available on the [GitHub Releases](https://github.com/jomvick/komet/releases) page.

### 🐧 Linux

**Option 1: One-line install (Recommended)**
```bash
curl -fsSL https://raw.githubusercontent.com/jomvick/komet/main/install.sh | sh
komet status
```
The installer configures `komet` in `~/.komet/app` and starts the user systemd service.

**Option 2: Standalone AppImage**
Download `komet-<version>-linux-x86_64.AppImage` from [Releases](https://github.com/jomvick/komet/releases):
```bash
chmod +x komet-*.AppImage
./komet-*.AppImage
```

**Option 3: Tarball**
Download and extract `komet-<version>-linux-<arch>.tar.gz` and run `./install.sh` inside the archive.

---

### 🍏 macOS

**Option 1: Disk Image (.dmg)**
Download `komet-<version>-macos-arm64.dmg` from [Releases](https://github.com/jomvick/komet/releases), open it and drag `Komet.app` to your `Applications` folder.

**Option 2: CLI & Background Daemon**
```bash
komet daemon install
komet status
```

---

### 🪟 Windows

**Option 1: Portable Executable / Zip**
Download `komet-<version>-windows-x86_64.exe` or `komet-<version>-windows-x86_64.zip` from [Releases](https://github.com/jomvick/komet/releases).
Extract and run `komet.exe`.

**Option 2: Background Service**
To install and run Komet as a background Windows service:
```powershell
komet.exe daemon install
# or
komet.exe --service
```

---

## 🚀 Day-to-Day Usage

```bash
komet status      # Check engine status and local/synced mode
komet update      # Check and update to the latest release
komet daemon start|stop|restart|status
```

---

## 🔄 Multi-Device Sync (Self-Hosted)

Komet runs **100% locally** by default. To sync across devices, self-host `komet-sync`:

```bash
komet sync-init                          # prints KOMET_SYNC_TOKEN
docker compose -f docker-compose.sync.yml up -d  # on your VPS
# or locally: KOMET_SYNC_TOKEN=xxx komet sync-server
```

Then on each device:
```bash
export KOMET_EDGE_URL=http://YOUR_VPS:8787
export KOMET_SYNC_TOKEN=xxx
komet
```

See [`docs/self-hosted-sync.md`](docs/self-hosted-sync.md) for details.

---

Developing or curious how it works? Check out [ARCHITECTURE.md](ARCHITECTURE.md).

Licensed under the [MIT License](LICENSE).
