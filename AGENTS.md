# AGENTS.md

Komet: Rust workspace (edition 2024, stable toolchain) — gpui UI + engine in one binary,
local-first with optional self-hosted sync. Read `ARCHITECTURE.md` for the design;
`docs/PARITY.md` has the per-feature status. Do not re-derive crate roles from filenames —
the crate table in `ARCHITECTURE.md` §3 is authoritative.

## Commands

- Build/run (two binaries, so `--bin` is required — plain `cargo run` fails):
  `cargo run --bin komet` (headed; `headless` subcommand = engine only),
  `cargo run --bin komet-sync-server`.
- Verify before a PR: `cargo check --workspace --all-targets`,
  `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all`, `cargo test --workspace`.
- One crate / one test (prefer this over the full workspace — gpui builds are slow):
  `cargo test -p komet-engine <testname>`, `cargo test -p komet-harness --test acp <testname>`.
- Integration tests live in `crates/{engine,harness,rpc,sync,syntax}/tests/`; `crates/ui` has
  only lib unit tests (verify UI changes with `cargo check -p komet-ui`).
- Many tests are `#[ignore]`d because they need a live server or a real agent CLI. Run them with
  `cargo test -p <crate> -- --ignored` after setting `KOMET_EDGE_WS` (e.g. `ws://127.0.0.1:8787`).
- Offline UI demo with a seeded mock engine: `scripts/dev-demo.sh` (`--slow` paces streaming).

## Toolchain gotchas

- gpui is pinned to a `jomvick/zed` git rev plus `[patch]` entries in the root `Cargo.toml`.
  Bumping the rev means updating every gpui* dep and patch together. Never use Zed's GPL crates
  (`markdown`, `ui`, `theme`, `editor`) — markdown/theme/components are Komet's own.
- The `[patch."https://github.com/wingleeio/zed"]` block is not a leftover: `gpui-component`
  (`jsgrrchg/gpui-component`) declares its gpui deps against `wingleeio/zed`, and the patch
  redirects them to the `jomvick/zed` fork so the workspace has a single gpui. Removing it
  yields two incompatible gpui crates. It only goes away if gpui-component is forked too.

## When to patch the gpui fork (`jomvick/zed`)

The fork is a maintenance cost (every upstream bump rebases its commits), so default to fixing
things in `crates/ui`. Patch the fork only when **all** of these hold:

1. Komet-side is impossible, not merely harder: the need is a renderer/platform primitive
   (new draw primitive, GPU memory release path, platform window/blur API, text-shaping rule)
   or a bug inside gpui itself. Element composition, caching, scheduling, layout tricks, and
   state management belong in `crates/ui`.
2. There is a measurement motivating it (`KOMET_GPU_STATS=1`, frame time, RSS per
   `docs/memory-plan.md`, or a reproducible visual bug) and the PR records before/after.
3. The change is minimal and self-contained: one commit per concern on the fork's `komet-gpui`
   branch, written to rebase cleanly on upstream (no reformatting, no drive-by refactors).

Procedure: land the commit on the fork first, then bump **every** `jomvick/zed` `rev` in the root
`Cargo.toml` (deps and both `[patch]` blocks) in one commit, and append a one-line summary of
the new fork commit to the comment above the `gpui =` line so the list stays authoritative.
Before writing a fork patch, check whether upstream Zed already has the fix — prefer cherry-picking
upstream over a bespoke change.
- Linux builds need gpui system libs (`libxkbcommon`, `libwayland`, `libx11-xcb`, `libfontconfig`,
  `libfreetype`, `libasound2`, `libvulkan`, `cmake`, `libfuse2`). The exact apt line is in
  `.github/workflows/release.yml`.
- Workspace dependencies and `version` are declared once in the root `Cargo.toml`; crate manifests
  reference them with `.workspace = true`. Add deps there first.

## Runtime / env

- `KOMET_DATA_DIR`, `KOMET_IPC_PORT`, `KOMET_HARNESS=mock` control local runs (used by the scripts).
- Sync is off by default. Enable with `KOMET_EDGE_URL` + `KOMET_SYNC_TOKEN`; the canonical token var
  is `KOMET_SYNC_TOKEN` — `KOMET_EDGE_TOKEN` is only the dev-bearer shortcut in `crates/engine`.
- Store roots differ by profile: local → `{data_dir}/profiles/local/`, synced/development →
  `{data_dir}/orgs/{org_id}/{user_id}/` (see `ARCHITECTURE.md` §"Local-first workspace profiles").

## Known traps

- `scripts/e2e-smoke.sh` is stale: it still starts the `edge/` Cloudflare Worker via `wrangler`,
  and `edge/` was removed (commit `bc3f8ab`). It will not run as-is — use `scripts/dev-demo.sh`.
- A new agent harness must declare its sandbox branch in the `ARCHITECTURE.md` §6 matrix and add a
  surface-declaration test (pattern: `cline_descriptor_surface_matches_registry_expectations` in
  `crates/harness/tests/acp.rs`); a silent no-op must not ship.
- komet sandbox settings constrain the *agent CLI's* own permissions, not an OS boundary — see
  `docs/security.md` before touching sandbox/permission code.
- Do not run the release packaging scripts for verification; CI (`.github/workflows/release.yml`)
  owns packaging. Release signing vars/secrets are required for tagged builds.

## Conventions

- Conventional Commits with a scope, e.g. `fix(models): ...` (see `git log`).
- `CONTRIBUTING.md`: no internal API freeze; prefer small PRs and discuss large refactors first.
- Docs mix French and English — match the language of the file you edit.
- The parent repo `AGENTS.md` (`../AGENTS.md`) mandates the Superpowers skill workflow
  (brainstorm → plan → TDD → verify). `.clinerules/workflows/` holds the check/fmt/test/PR recipes.
