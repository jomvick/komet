# OpenCode agent over ACP — design

Date: 2026-08-15
Status: Approved (brainstorming) — implementation order: harness + proto first, then UI + docs.

## Summary

Add the opencode CLI as a supported agent in zeron/comet, driven over the
Agent Client Protocol. opencode ships a **native ACP server** (`opencode acp`,
JSON-RPC stdio), so no external adapter is needed — the same shape as Cursor,
Grok and Hermes, which are also native ACP.

Scope is **minimal parity** (the harness + registry + settings-surface pattern
every agent follows). No account/login integration: auth is managed by opencode
itself (`opencode auth login` — the authMethod opencode advertises over ACP is
`opencode-login`). All advertised slash commands (skills) are shown.

## Verified facts (live probe of `opencode acp`, v1.18.18)

- `opencode acp` is a native ACP server (JSON-RPC over stdio). `initialize`
  replies `protocolVersion: 1`, `loadSession: true`, mcp http+sse, prompt
  embeddedContext+image, session close/fork/list/resume, agentInfo
  "OpenCode" 1.18.18.
- **No steering extension** is advertised → steers deliver at **turn boundaries**
  (`SteeringMode::TurnBoundary`).
- `session/new` requires `mcpServers: []` (else `-32602 Invalid input`).
- `session/new` returns **`configOptions`** (not first-class `models`/`modes`):
  a `model` select (OpenCode Zen models — `opencode/big-pickle` default,
  `opencode/deepseek-v4-flash-free`, `hy3-free`, `laguna-s-2.1-free`,
  `mimo-v2.5-free`, `nemotron-3-ultra-free`, `nemotron-3.5-lightning-free`)
  plus a `mode` select (`build`/`plan`). There is **no `thought_level`** option,
  so the reasoning ladder is empty. The wire always wins over any static
  catalog; user-configured providers also surface here.
- Emits `available_commands_update` with the slash-command/skill set.
- `authenticate` is a stub; terminal auth via `opencode auth login`.

Install surface on this device: `~/.opencode/bin/opencode` (official installer);
also available via npm (`opencode-ai`) and homebrew.

## Design

### 1. Identity — `crates/proto/src/agent.rs`

Add a variant to `HarnessId` (serde `rename_all = "kebab-case"` → `"opencode"`):

```rust
/// The opencode CLI's native ACP server (`opencode acp`).
OpenCode,
```

No new `ReasoningLevel`: opencode advertises no effort over ACP.

### 2. Spec — `crates/harness/src/acp/mod.rs`

`fn opencode_spec() -> AcpAgentSpec`, modelled on `cursor_spec()`/`hermes_spec()`:

- `id: HarnessId::OpenCode`, `display_name: "OpenCode"`
- `executable: "opencode"`, `args: ["acp"]` — native ACP, **no npm adapter**
  (`npx_package: None`; `resolve_launch` already handles the binary→npx
  fallback and simply has no npx branch here).
- `env_override: Some("OPENCODE_EXECUTABLE")`
- `extra_paths`: `~/.opencode/bin` (official installer) plus the standard npm /
  homebrew dirs. The existing `find_on_paths` helper already merges PATH +
  login-shell + node-version-manager bins (`node_version_manager_bins()`);
  opencode only needs its own installer dir prepended.
- `steering_mode: SteeringMode::TurnBoundary`
- `reasoning_levels: &[]`
- `prompt_transform` / `effort_values`: identity / default.
- `models: || Vec::new()` — **wire-first**. `models_from_session` (mod.rs
  ~l. 763) already maps `configOptions` into `Model`s: the `model` select
  becomes the model list, and the generic config-option mapping lifts the
  `mode` select (`build`/`plan`) into a `ModelOption`. No change to that code.
- Constructor `pub fn opencode() -> Self` + doc comment, following the
  existing constructors (`cursor()`, `grok()`, `hermes()`, …).

### 3. Registry — `crates/engine/src/registry.rs`

Lazy slot in `default_registry()`, mirror of the spec (pattern of Cursor/Grok/
Hermes/Pi — opt-in, `enabled: None`):

```rust
registry.register_lazy(
    HarnessDescriptor {
        id: HarnessId::OpenCode,
        name: "OpenCode".into(),
        supports_steering: true,
        steering_mode: SteeringMode::TurnBoundary,
        reasoning_levels: Vec::new(),
        installed: true,
        enabled: None,
    },
    Box::new(|| zeron_harness::AcpHarness::opencode().installed()),
    Box::new(|| Ok(Arc::new(zeron_harness::AcpHarness::opencode()) as Arc<dyn Harness>)),
);
```

Update the registry tests that assert the full slot list and resolve/enable
coverage (`HarnessId::OpenCode` added to the expected vectors).

### 4. Auth — no integration

opencode has no zeron account. The Accounts page does **not** list it; the
`provider_icon` match in `crates/ui/src/settings/accounts.rs` already has a
`_ =>` fallback, so **no change** is required there. Auth is managed by
opencode (`opencode auth login`).

### 5. UI — Settings → Agents

`crates/ui/src/settings/harnesses.rs`, exhaustive matches to extend:

- `blurb()`: "The opencode coding agent, driven through the opencode CLI."
- `cli_name()`: `"opencode"`

The opt-in toggle + not-installed hint render automatically via the generic
row pattern (no further change).

### 6. UI — icons

- `harness_brand_icon()` (`crates/ui/src/pickers.rs` ~l. 3283): add arm
  `HarnessId::OpenCode => (crate::icons::OPENCODE_MARK, None)`.
- `crates/ui/src/icons.rs`: register `OPENCODE_MARK = "opencode-mark"` in
  `icon_assets!` and add the SVG asset
  `crates/ui/assets/icons/opencode-mark.svg` (monochrome mark, like
  Grok/Hermes/Pi).

No touch to the accounts page icon logic (fallback `_` arm covers it).

### 7. Docs

- `README.md` l. 3: add OpenCode to the agent list.
- Doc header of `crates/harness/src/lib.rs` (~l. 4-10): mention opencode as a
  native ACP agent.

## Tests — `crates/harness/tests/acp.rs`

Following the existing per-agent test pattern:

- `installed()` probe: PATH resolution, `~/.opencode/bin`, and the
  `OPENCODE_EXECUTABLE` override.
- `launch_program()` → `opencode` + `["acp"]`.
- `models_from_session` with mocked `configOptions`: OpenCode Zen models +
  the `mode` config option (build/plan) surfaced as a `ModelOption`; empty
  reasoning ladder.
- `steering_mode` is TurnBoundary.
- `commands` from a mocked `available_commands_update` (all shown).
- `OPENCODE_EXECUTABLE` fixture points at a fake binary so tests never depend
  on a real opencode install.

## Explicitly out of scope

- No account/login integration (auth via `opencode auth login`).
- No static model catalog (wire-first only).
- No mode-specific UI (build/plan surfaces only via the generic config-option
  picker).
- No changes to `models_from_session`, `find_on_paths`, `resolve_launch`,
  `agent_accounts.rs`, the edge, or the RPC plane. The run flow is 100 %
  generic.