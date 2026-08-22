# Komet

Control your coding agents (Claude Code, Codex, Cursor, Grok, Hermes, OpenCode, Pi) locally by default, with optional multi-device sync.

Every device runs a small engine that stores sessions on that device. A new installation starts in local-only mode without an account or a network connection.

## Install and run locally (Linux)

```bash
curl -fsSL https://komet.sh/install.sh | sh
komet status
```

The installer starts the daemon immediately and keeps it running across reboots. No sign-in or sync configuration is required.

Day-to-day:

```bash
komet status      # local/synced mode and engine status
komet update      # update to the latest release
komet daemon start|stop|restart|status
```

## Multi-device sync (self-hosted)

Komet runs **100% locally** by default. To sync between devices, self-host `komet-sync`:

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

See `docs/self-hosted-sync.md` for details. Legacy Cloudflare/WorkOS `edge/` is deprecated.

On macOS: use the desktop release, or build `komet` from source and run `komet daemon install` to install the launchd service.

## Install on Windows

Download the latest `.msi` installer from the [GitHub release](https://github.com/opencode/komet/releases) and run it — it installs `komet.exe` into `Program Files\Komet`, registers uninstall keys, and optionally installs the `Komet` Windows service (running as the logged-in user). A standalone `komet.exe` portable executable is also available in the release assets; no service is registered for the portable variant.

To install the service from the command line:

```powershell
c:\Program Files\Komet\komet.exe --service
```

or equivalently via `sc`:

```powershell
sc create Komet binPath= "\"c:\Program Files\Komet\komet.exe\" --service"
```

Day-to-day management:

```powershell
komet daemon start
komet daemon stop
komet daemon restart
komet daemon status
```

## Install and run locally (Linux)

```bash
curl -fsSL https://komet.sh/install.sh | sh
komet status
```

---

Developing or curious how it works? Check out [ARCHITECTURE.md](ARCHITECTURE.md).

Licensed under the [MIT License](LICENSE).
