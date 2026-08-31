# Claude Code — Built-in commands & bundled skills inventory (native access in komet)

Source: `code.claude.com/docs/en/commands`, `code.claude.com/docs/en/skills`, `platform.claude.com/docs/en/agent-sdk/slash-commands` (fetched 2026-08-30). Companion to `docs/research/harness.md`. Scope: **Claude Code only**, per the current priority.

## How Claude Code exposes these today (and what komet already sees)

Two channels exist, and komet's `ClaudeHarness` only speaks one of them:

1. **`initialize` control_request → `response.commands`** (name / description / argumentHint). This is exactly what `ClaudeHarness::discover_commands()` already reads, cached into `slash_cache` and shown in the composer's `/` popup. Per the Agent SDK docs, this list mixes **built-in commands**, **bundled skills**, and **custom commands** together, by name only — no flag saying "this one opens a dialog" or "this one is TUI-only".
2. **Sending `"/name args"` as a literal first line of a prompt.** The CLI's own input loop intercepts a leading `/` before treating it as a chat turn — this is how the Agent SDK docs show `/compact` and `/clear` being driven programmatically, and it's the same `--input-format stream-json` stdin komet already writes to in `run()`.

There's also a **third channel** we're not using at all: per `docs/research/harness.md`'s own protocol notes, the control channel already speaks `set_permission_mode`, `set_model`, `rewind_files`, `mcp_reconnect`/`toggle`/`status`, `get_context_usage`, and `stop_task` as dedicated control_request verbs — structured request/response, no prompt-text ambiguity. `discover_commands()` only ever sends `initialize` on that channel; the rest is untouched.

### Two things that must be verified live before any driver code is written

1. **Does `initialize`'s `commands` list in `--print --input-format stream-json` mode match the interactive TUI's `/` menu, or a smaller non-interactive subset?** Dialog-only commands (`/theme`, `/permissions` with no args, `/mcp` with no args) plausibly get filtered or degrade to a no-op when driven headlessly — undocumented either way.
2. **What frame shape does the CLI emit when a builtin actually runs via a stdin-sent `/name` line?** `Normalizer::normalize()` in `crates/harness/src/claude/normalize.rs` today only handles `system` subtypes `init`, `task_started`, `task_notification` — every other `system` subtype (a plausible home for something like `compact_boundary`) hits the catch-all `if f.subtype != "init" || self.saw_init { return Vec::new(); }` and is **silently dropped**. A `/compact` sent today may already "work" on the CLI side and vanish before it reaches komet's UI.

Neither is knowable from docs alone — they're the first probes to run once coding starts, not something to guess into this inventory.

## Bucket A — plain text-turn semantics, no interactive dialog (best first targets via channel 2)

Context / session management:
- `/clear [name]` (aliases `/reset`, `/new`) — redundant with komet's own new-chat, but cheap
- `/compact [instructions]`
- `/context [all]` — output is a "colored grid" in the TUI; text-mode shape unverified
- `/cost` (alias of `/usage`)
- `/rewind` (aliases `/checkpoint`, `/undo`) — also has a `rewind_files` control_request (see Bucket C)
- `/diff`
- `/branch [name]`
- `/resume [session]` (alias `/continue`) — komet has its own session list; likely skip
- `/rename [name]`
- `/export [filename]`
- `/plan [description]`
- `/goal [condition|clear]`
- `/btw [question]`
- `/add-dir <path>`
- `/cd <path>`
- `/subtask` — in-session subagent, no new host machinery needed
- `/background [prompt]`, `/fork [prompt]` — spawn new (background) sessions; overlaps komet's own multi-session model, needs a design call, not just a wire call
- `/config key=value` (alias `/settings`) — the scriptable form only; the bare dialog form is Bucket B
- `/mcp reconnect <server>` / `enable`/`disable` — scriptable subcommands only; bare dialog is Bucket B (also see Bucket C, `mcp_reconnect`/`toggle`/`status`)
- `/permissions` — dialog by default; also see Bucket C, `set_permission_mode`
- `/init`, `/import [codex|gemini]`, `/memory`
- `/hooks`, `/insights`, `/recap`, `/release-notes`
- `/reload-plugins`, `/reload-skills`
- `/rate-limit-options`, `/sandbox`
- `/schedule [description]` (alias `/routines`) — cloud-side routines; may not map to komet's own scheduling story
- `/agents` — as of v2.1.198 just prints a reminder to ask Claude directly; low value as a dedicated command

Bundled skills (already prompt-shaped by design, so these should just work once discovery surfaces them):
`/batch`, `/code-review` (alias `/review`), `/security-review`, `/doctor` (alias `/checkup`), `/debug`, `/run`, `/run-skill-generator`, `/verify`, `/loop` (alias `/proactive`), `/claude-api`, `/dataviz`, `/design-sync`, `/fewer-permission-prompts`, `/simplify`, `/deep-research` (workflow, not skill, same shape).

## Bucket B — TUI-only / interactive dialogs / no server-side meaning (skip)

`/vim`, `/theme`, `/color`, `/scroll-speed`, `/keybindings`, `/focus`, `/ide`, `/desktop` (alias `/app`), `/radio`, `/powerup`, `/mobile` (aliases `/ios`, `/android`), `/passes`, `/heapdump`, `/login`, `/logout` (komet manages its own account swap), `/chrome`, `/install-github-app`, `/install-slack-app`, `/privacy-settings`, `/remote-control` (alias `/rc`), `/remote-env`, `/teleport`, `/list-agents` (alias `/peers`), `/exit` (alias `/quit`), `/help` (komet has its own UI for this), `/bug` (alias `/share`), `/feedback`, `/artifacts`, `/auto-mode-setup`, `/autocompact`, `/autofix-pr`, `/fast`, `/advisor`, `/design-login`, `/copy` (clipboard — meaningless server-side; komet already has its own copy affordance).

## Bucket C — dedicated control_request verbs (bypass the text-command channel entirely)

Already-known verbs on the same control channel `discover_commands()` speaks, per `docs/research/harness.md`:

| Verb | Maps to slash command | Why prefer this over sending `/name` as text |
|---|---|---|
| `set_permission_mode` | `/permissions` | Structured mode value, no dialog to parse out of stdout |
| `set_model` | `/model` | komet already has its own model picker RPC; this could let a run change model mid-session without a respawn |
| `rewind_files` | `/rewind` | Structured checkpoint id, not a TUI list to navigate |
| `mcp_reconnect` / `toggle` / `status` | `/mcp` | Structured server name + action, no interactive list |
| `get_context_usage` | `/context` | Structured numbers, not a rendered grid |
| `stop_task` | (background-task stop, no single slash equivalent) | Structured task id |

These six are the **highest-value first targets**: no prompt-injection ambiguity, no guessing at output frame shape, and they don't fit the existing `SlashCommand` proto type at all (that type is "typed at the start of the composer, sent as prompt text" — these are host-side actions with structured request/response, closer in shape to `RunControls::request_permission` than to a `SlashCommand`).

## Open design question this inventory surfaces

Bucket A/B entries are genuinely "type `/name`, it's a prompt line" — they fit the existing `SlashCommand` → composer `/` popup path once discovery/normalizer gaps are closed. Bucket C entries are not slash commands to komet at all; they're **actions** (a settings toggle, a checkpoint picker, an MCP server list) that happen to also have a `/name` alias in the original CLI. Modeling them as new proto types (e.g. `HarnessAction` alongside `SlashCommand`) rather than shoehorning them into the composer's slash popup is probably right, but that's a decision for the next session, not this inventory.

## Suggested next steps (empirical, before writing driver code)

1. Spawn `claude --print --input-format stream-json --output-format stream-json --verbose`, read the `initialize` `commands` list, and diff it against the interactive TUI's `/` menu (plain `claude`, no `--print`) — settles the real Bucket A/B boundary for edge cases like `/context` and `/mcp`.
2. Send a raw `/compact` and `/clear` as the first stdin line and capture the exact stdout frames — confirms whether `Normalizer` needs a new arm and what shape it needs.
3. Probe each of the six Bucket C verbs against a live CLI (`set_permission_mode`, `set_model`, `rewind_files`, `mcp_reconnect`, `get_context_usage`, `stop_task`) and record exact request/response JSON — this becomes the actual driver PR.
4. Only after 1–3: decide the proto/UI shape (extend `SlashCommand`, or add a new `HarnessAction` type + UI surface for Bucket C).
