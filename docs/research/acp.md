# ACP integration: shared harness + Grok Build (2026-08)

## Decision
- Add an **ACP harness** (`crates/harness/src/acp/`) speaking Agent Client Protocol
  v1 — JSON-RPC 2.0 newline-framed over stdio, same wire shape as the codex
  app-server — over the shared `crates/harness/src/jsonrpc.rs` client (promoted
  from `codex/rpc.rs`). Wire types are hand-rolled tolerant serde against raw
  `Value`s (house style, verified against `agent-client-protocol-schema` 1.3.0),
  NOT the official SDK crates: komet keeps its own child-lifecycle hardening
  (StderrTail, SIGTERM→SIGKILL, PATH composition) and shell-script test
  fixtures, and drives raw updates the SDK's `ActiveSession` abstraction hides.
- First registered agent: **Grok Build** (`grok agent stdio`), xAI's native ACP
  agent (npm `@xai-official/grok`, ACP registry id `grok-build`). Auth: browser
  OAuth or `XAI_API_KEY`; komet passes env through. `GROK_EXECUTABLE` overrides
  resolution (tests point it at `tests/fixtures/fake-acp.sh`).
- **claude/codex keep their custom adapters.** ACP parity would cost the
  `--settings` passthrough (fastMode/ultracode), pin adapter-bundled agent
  versions over the user's installed CLIs, and change Claude steering semantics
  (the adapter's `_session/steering` pre-empts with priority `now` vs our
  step-boundary stdin line). Revisit when ACP v2 stabilizes (prompt lifecycle,
  first-class permissions, resume-with-replay) and per-turn usage leaves the
  unstable flag.

## Protocol surface used (v1)
- `initialize` (protocolVersion 1; fs/terminal client capabilities declined) →
  `session/new` / `session/load` (fresh-session fallback; replay drained, the
  doc already holds history) → `session/prompt` per turn; the prompt RESPONSE
  carries the `stopReason` (`cancelled` → Interrupted, `refusal` → Errored,
  else Completed). `session/cancel` is the interrupt; SIGTERM/SIGKILL escalate.
- `session/update` notifications → `AgentEvent`: message/thought chunks →
  Text/ReasoningDelta; `tool_call`/`tool_call_update` → typed ToolCall (kind +
  rawInput + locations + diff content) + ToolResult carrying **capped output
  text and inline diffs** (16KB/64KB harness caps; 4KB/16KB doc caps in
  `parts.rs` — the session-load-size discipline); `plan` → `ToolCall::Todo`
  (stable id `acp-plan`); `available_commands_update` →
  `AgentEvent::AvailableCommands`. `usage_update` is a context gauge, not
  per-turn tokens — deliberately unmapped.
- `session/request_permission` → auto-accept the preferred allow option
  (`allow_always` > `allow_once` > first) — parity with claude
  bypassPermissions / codex approvalPolicy never.
- **Session config options**: ACP has no per-prompt model field; the run's
  model + reasoning apply through `session/set_config_option` against the
  session response's advertised `configOptions` (category `model` /
  `thought_level`, matched to advertised value ids, skipped when current,
  never fatal). Grok's effort ladder in the picker is Low/Medium/High →
  `low`/`medium`/`high`; other komet levels degrade down a preference ladder
  (`config_option_sets`).
- Steering: `_session/steering` extension when
  `initialize._meta.steering.supported` (org adapters); request carries
  `_meta.steering.idleBehavior: "promptRequired"` so a turn-end race hands the
  text back instead of firing an untracked turn. Without the extension (Grok):
  queue and deliver as the next `session/prompt` — `SteeringMode::TurnBoundary`.
  Session parks between turns while the steering mailbox lives (codex pattern).
- Ordering hazard fixed twice: responses resolve via the pending map while
  notifications ride the incoming channel, so (a) `request_draining` flushes
  the channel after `session/load` resolves, (b) the turn arm drains queued
  updates before emitting Done, (c) EOF right after a final response reads as
  a clean finish (50ms turn-future grace), not a crash.

## New shared surface
- `Harness::commands()` (default empty) + `ListCommands` RPC (mirrors
  ListModels, relay-forwardable) → composer `/` popup (mirrors the file-mention
  popup; local `filter_indices` ranking, no per-keystroke RPC).
- `AgentEvent::ToolResult{output?, diff?}` → `MessagePart::Tool{output?, diff?}`
  → doc columns (`output`, `diff` — additive, TS mirror updated in
  render-parts.ts/control-types.ts) → expandable transcript chips
  (`tool_detail_lines`: `similar` line diff, context collapsed to `⋯`, 12-line
  cap, analytic heights).

## Citations
agentclientprotocol.com (v1 spec + schema), agentclientprotocol org repos
(claude-agent-acp v0.66.0, codex-acp v1.1.14 — steering wire shape),
agent-client-protocol-schema 1.3.0 (serde tags), ACP registry entry
`grok-build`, live `grok agent stdio` initialize handshake (2026-08-07).
