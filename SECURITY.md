# Security Policy

## Supported versions

Komet is in **alpha/beta** — see the [README](README.md#status). Only the
latest release receives security fixes; upgrade rather than patching old
builds.

## Reporting a vulnerability

**Please do not open a public issue for security reports.**

Use GitHub's private vulnerability reporting:
**[Security → Report a vulnerability](https://github.com/jomvick/komet/security/advisories/new)**

Include, where possible:

- affected version / commit
- reproduction steps or proof of concept
- impact assessment (what an attacker gains)

You can expect an acknowledgement within a few days. Please give us a
reasonable window to investigate and ship a fix before any public disclosure.

## Scope notes

- **Komet's sandbox settings are not an OS boundary.** They configure the
  agent CLI's own permissions only — agent processes run with your user's
  privileges on the host. See [docs/security.md](docs/security.md) before
  reporting anything in that area as a vulnerability: it is documented
  behavior, not a bug.
- Sync (`komet-sync-server`) authenticates with a single bearer token per
  deployment. Reports about token handling, room isolation between
  namespaces, or blob path handling are in scope.
- Issues in upstream agent CLIs (Claude Code, Codex, Cursor, …) should be
  reported to those projects, not here — unless Komet mishandles their
  output in a way that creates a vulnerability.
