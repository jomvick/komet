# Roadmap

Komet is in **alpha → beta**. This roadmap is deliberately short and honest —
items move only when they ship. Feature-level truth lives in
[docs/PARITY.md](docs/PARITY.md); architecture decisions in
[ARCHITECTURE.md](ARCHITECTURE.md).

## Now — alpha → beta

- **Stabilize the core loop** across all 13 agents: run, steer, interrupt,
  questions, diffs, terminals, provider switching.
- **ACP Gate B evidence** (`docs/research/acp.md`): collect live proof per
  agent before enrolling any of them in the elicitation/compaction
  allowlists — no silent no-ops.
- **Engine hardening**: parent-PID watchdog, boot-time warm-open of recent
  chats.
- **Packaging**: signed macOS build, Windows installer polish, Linux
  arm64 coverage.
- **Docs**: keep PARITY.md and the interaction-surface matrix in sync with
  every harness change.

## Next

- **Memory**: per-device project memory with FTS5 search and deterministic
  handoff briefings already ships (`crates/memory`, issue #78). Next:
  recall quality, retention controls, and deciding whether any memory tier
  should ever cross the sync boundary.
- **Remote environments**: harden `komet serve` (pairing, TLS, multi-user
  namespaces) so a VPS can host a full workspace.
- **More agents**: evaluate new CLIs against the §6 access-mapping rule
  before adding a harness.
- **Performance**: frame-budget baselines in CI (`scripts/frame-budget.sh`).

## Later

- **Mobile companion** (out of scope so far — see PARITY.md deferrals).
- **End-to-end encryption** for synced docs (transport is currently TLS +
  bearer token only).
- **Hosted sync option** — only if self-hosting proves to be a real barrier;
  local-first stays the default either way.

## How to influence it

Open an issue or a [Discussion](https://github.com/jomvick/komet/discussions).
Large changes are discussed before code — see
[CONTRIBUTING.md](CONTRIBUTING.md).
