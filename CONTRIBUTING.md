# Contributing to Komet

Thanks for your interest in Komet — a multi-agent desktop control plane (Claude Code, Codex, Cursor, Grok, Hermes, Pi, and others) written in Rust/GPUI.

## Current project state

Komet is in **active, fast-moving development**. The internal architecture (proto, harnesses, frame normalization, UI) still changes often — this is **not** a stabilized project, and there is no internal API freeze.

Concretely, that means:

- Some parts of the code can change shape from one week to the next (e.g. new harnesses, new frame types).
- There may be known-fragile areas that are not fully traced in the code (e.g. call sites that were not rediscovered, behavior that must be revalidated after a payload schema change).
- The most useful contributions right now are those that **advance an in-progress effort** or **fix a known bug**, rather than large refactors or architecture changes that were not discussed first.

## Before you contribute

1. **Open an issue or discuss a large change first.** Given how quickly the code moves, an undiscussed bulky PR is likely to collide with work already in flight.
2. **Prefer small PRs.** Easier to review and land while the architecture is still moving.
3. **Describe the context, not just the diff.** Why this change, what behavior you saw before/after, and how you tested it.

## Welcome contributions

- Bug fixes for observed, reproducible behavior.
- New harnesses / agent integrations, following the existing harness pattern (proto → normalization → UI).
- Tests covering frames / normalization (historically a source of silent bugs).
- Documentation and clarification of existing code, especially where the data flow (UI ↔ engine ↔ proto) is not obvious.

## Premature for now

- Large refactors of UI or proto modules without prior discussion.
- Major dependency or architecture changes (e.g. sync/auth backend) without prior discussion — several options are still under consideration and not decided.
- Performance optimizations on code that is still changing quickly.

## Code style

- Idiomatic Rust; `cargo fmt` and `cargo clippy` clean before a PR.
- No new dependency without a clear justification in the PR description.
- Match the patterns already in `crates/ui/` and `crates/engine/` rather than introducing a new style.

## Reporting a bug

Please include:

- Precise reproduction steps.
- Expected vs observed behavior.
- Platform (Linux/Windows/macOS) and desktop environment if relevant (Komet is used as a floating window under GNOME, for example).
- A screenshot if the bug is visual.

## Questions

If the code seems inconsistent with what you see at runtime (that happens — some areas are leftover scaffolding that has not been cleaned up yet), open an issue to clarify before building on an assumption.
