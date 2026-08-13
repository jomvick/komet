# Komet

Control your coding agents (Claude Code, Codex, Cursor, Grok, Hermes, Pi) from
any of your devices.

![Komet driving a Claude Code session with a live branch diff sidebar](apps/landing/public/assets/app-screenshot.jpg)

Every device runs a small engine that keeps your sessions in sync: start an
agent on one machine, follow and drive it from another. Install the engine as
a daemon on an always-on machine (a VPS, a spare box) and your agents keep
working after you close your laptop.

## Install the daemon (Linux)

```bash
curl -fsSL https://komet.komet.sh/install.sh | sh
komet login                          # sign in (paste a code, done)
systemctl --user start komet-native
```

No configuration needed. Day-to-day:

```bash
komet status      # signed in? engine running?
komet update      # update to the latest release
komet daemon start|stop|restart|status
```

On macOS: build `komet` from source, then `komet daemon install` (launchd).

---

Developing or curious how it works? [![Ask DeepWiki](https://deepwiki.com/badge.svg)](https://deepwiki.com/komet/komet) or check out [ARCHITECTURE.md](ARCHITECTURE.md).

Licensed under the [MIT License](LICENSE).
