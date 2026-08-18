# Komet

Control your coding agents (Claude Code, Codex, Cursor, Grok, Hermes, OpenCode, Pi) locally by default, with optional multi-device sync.

Every device runs a small engine that stores sessions on that device. A new installation starts in local-only mode without an account or a network connection.

## Install and run locally (Linux)

```bash
curl -fsSL https://komet.sh/install.sh | sh
komet status
```

The installer starts the daemon immediately and keeps it running across reboots. No sign-in or sync configuration is required.

Prefer a portable build? Grab the `.AppImage` from the [GitHub release](https://github.com/jomvick/komet/releases), `chmod +x` it and run it — nothing is installed system-wide.

Day-to-day:

```bash
komet status      # local/synced mode and engine status
komet update      # update to the latest release
komet daemon start|stop|restart|status
```

## Multi-device sync (future)

Komet currently runs **100% locally**: no account, no login screen, no network
calls. Every session, attachment and diff stays on the machine that created it.

Multi-device sync is part of the app's roadmap. The codebase already contains
the building blocks — the `edge/` sync worker (Loro CRDT rooms, device relays,
R2 attachments) and the WorkOS auth routes — but they are disabled by default.
To enable sync later:

1. Deploy the `edge/` worker to your own Cloudflare account and create a WorkOS
   AuthKit app (set the real client id in `edge/wrangler.jsonc`).
2. Run the engine with `KOMET_WORKOS_CLIENT_ID=<your client id>`, and
   `KOMET_EDGE_URL=<your edge host>` if you self-host instead of using the
   default edge endpoint.

With sync enabled you could start an agent on one device and follow or drive it
from another; an always-on machine such as a VPS can keep those agents working
after you close your laptop. Until then, `komet login` just reports
"dev mode — there is nothing to sign in to" and the app never leaves the machine.

On macOS: use the desktop release, or build `komet` from source and run `komet daemon install` to install the launchd service.

## Install on Windows

Download the latest `.msi` installer from the [GitHub release](https://github.com/jomvick/komet/releases) and run it — it installs `komet.exe` into `Program Files\Komet`, adds it to `PATH` and registers uninstall keys. A standalone `komet.exe` portable executable is also available in the release assets.

The `Komet` Windows service (running as the logged-in user) is not registered by the installer; add it with:

```powershell
komet daemon install
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
