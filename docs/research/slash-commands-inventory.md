# Claude Code — Built-in commands & bundled skills inventory (native access in komet)

Source: `code.claude.com/docs/en/commands`, `code.claude.com/docs/en/skills`, `platform.claude.com/docs/en/agent-sdk/slash-commands` (fetched 2026-08-30). Companion to `docs/research/harness.md`. Scope: **Claude Code only**, per the current priority.

## Live probe results (2026-08-31)

Ran `scripts/probe-claude-commands.py` against the real CLI (see that file for the exact requests). Findings, in order of how much they change the plan:

1. **`discover_commands()`'s data source is empty on this CLI version.** The `initialize` control_request sent with no prior turn returned **0 commands** in `response.commands`. Whatever schema `ClaudeHarness::discover_commands()` was written against, this CLI doesn't populate that field the same way anymore (or it never populates it without a live session — untested). Either way, **the `/` composer popup is getting nothing from Claude Code right now**, silently — no error, just an empty list.

2. **A much better command source exists: `get_context_usage`.** Its response includes `skills.skillFrontmatter[]`, and each entry has a `source` field — `"built-in"` for the CLI's own bundled skills, `"userSettings"` for user/plugin-installed ones. On this machine `"built-in"` includes `dataviz`, `update-config`, `keybindings-help`, `simplify`, `fewer-permission-prompts`, `loop`, `claude-api`, `run`, `init`, `review`, `security-review` — a real, tagged, disambiguated list, unlike `initialize`'s. **This should replace (or supplement) `initialize` as the discovery source** — filter `skillFrontmatter` by `source == "built-in"` for the composer popup, and treat `"userSettings"` entries as the project/personal custom commands the popup already wants to show.
   - Caveat: this list is *loaded* skills for the current session, not necessarily the full built-in catalog (`totalSkills: 835` on this machine includes hundreds of `userSettings` cybersecurity skills unrelated to Claude Code itself — this machine has an unusually large personal skill library installed globally). Filtering on `source == "built-in"` is what makes this usable regardless.
   - `get_context_usage` request shape confirmed: `{"subtype": "get_context_usage"}`, no other fields needed.

3. **`/compact` sent as a prompt line did NOT emit a `compact_boundary` frame** — just the ordinary `system:init` → `assistant` → `result` triplet. This matches the documented edge case (nothing to compact yet ⇒ no `compact_boundary`, the CLI just reports the reason in `result.result`), so it's inconclusive on its own: **needs a re-run with a non-trivial conversation already in context** to see the real `compact_boundary` shape and confirm whether `Normalizer` needs a new arm for it.

4. **`/clear` sent as a prompt line produces a frame type nobody knew about: `conversation_reset`** (`{"type":"conversation_reset","new_conversation_id":...,"session_id":...}`), immediately followed by a fresh `system:init` carrying a **different** `session_id`. This is a confirmed, concrete bug in the current driver: `Normalizer`'s `saw_init` dedup (meant to swallow a same-session background-subagent wake-turn init) would have silently swallowed this second init too — komet's UI would never learn `/clear` ran. **Fixed in this session**: `crates/harness/src/claude/wire.rs` gained a `Frame::ConversationReset` variant, and `crates/harness/src/claude/normalize.rs` clears `saw_init` (plus the subagent bookkeeping maps and rotates the assistant message id) on that frame, so the following init is honored as a genuine new `SessionStarted`. Tests added in both files. **Not yet re-verified against the live CLI** — the fix is built from the probe's frame *shape*, not a full round-trip; re-run the probe (or a real komet session) after building to confirm.

5. **Bucket C verbs — confirmed request/response shapes:**

| Verb | Request sent | Result |
|---|---|---|
| `get_context_usage` | `{"subtype":"get_context_usage"}` | Success — rich payload: token grid, `model`, `memoryFiles`, `mcpTools`, `agents`, `skills.skillFrontmatter[]` (see #2), `autoCompactThreshold`, `messageBreakdown`. Ready to drive `/context` as a Bucket C action. |
| `set_permission_mode` | `{"subtype":"set_permission_mode","mode":"default"}` | Success — `{"mode":"default"}` echoed back. Confirmed shape. |
| `set_model` | `{"subtype":"set_model","model":"sonnet"}` | Success — bare, no payload. Confirmed shape. |
| `mcp_reconnect` | `{"subtype":"mcp_reconnect","action":"status"}` (guessed) | **Error**: `"Server not found: undefined"`. The guessed `action`-based shape is wrong — this subtype likely always expects a real `server` name, and "status" (no-arg listing) may be a *different* subtype entirely, not an `action` value. Needs a retest naming a real connected MCP server, and a search for whatever subtype lists servers without one. |
| `rewind_files` | `{"subtype":"rewind_files","checkpoint":"latest"}` | **Error**: `"File rewinding is not enabled."` — gated behind a setting/flag not yet identified (possibly a `--settings` key, or requires the session to have made file edits / checkpointing turned on first). Payload shape unconfirmed either way. |
| `stop_task` | `{"subtype":"stop_task","task_id":"nonexistent"}` | Success — bare `{}`, even for a bogus id (not validated, or it's a fire-and-forget best-effort stop). Needs a retest against a REAL spawned background task id to see the shape when it actually stops something. |

## How Claude Code exposes these today (and what komet already sees)

Two channels exist, and komet's `ClaudeHarness` only speaks one of them:

1. **`initialize` control_request → `response.commands`** (name / description / argumentHint). This is exactly what `ClaudeHarness::discover_commands()` already reads, cached into `slash_cache` and shown in the composer's `/` popup. **Confirmed empty on the live CLI (see probe finding #1) — this channel needs to be replaced or supplemented, not just trusted.**
2. **Sending `"/name args"` as a literal first line of a prompt.** The CLI's own input loop intercepts a leading `/` before treating it as a chat turn. Confirmed working for `/clear` (produces `conversation_reset` + fresh init) and effectively for `/compact` (recognized, just had nothing to compact in an empty session).

There's also a **third channel**, confirmed live in this session: control_request verbs beyond `initialize` — `set_permission_mode`, `set_model`, `get_context_usage`, `stop_task` all answered correctly; `mcp_reconnect` and `rewind_files` answered but with errors from wrong assumptions about their shape (see probe table above).

## Bucket A — plain text-turn semantics, no interactive dialog (best first targets via channel 2)

Context / session management:
- `/clear [name]` (aliases `/reset`, `/new`) — **driver-side normalizer gap now fixed** (see probe finding #4); redundant with komet's own new-chat otherwise
- `/compact [instructions]` — recognized by the CLI; `compact_boundary` frame shape still needs a real (non-empty) conversation to observe
- `/context [all]` — output is a "colored grid" in the TUI; **superseded by the `get_context_usage` control_request, which returns the same data structured** — prefer that over sending this as text
- `/cost` (alias of `/usage`)
- `/rewind` (aliases `/checkpoint`, `/undo`) — also has a `rewind_files` control_request (see Bucket C); currently errors with "not enabled"
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
- `/mcp reconnect <server>` / `enable`/`disable` — scriptable subcommands only; bare dialog is Bucket B (also see Bucket C, `mcp_reconnect` — shape still unconfirmed)
- `/permissions` — dialog by default; also see Bucket C, `set_permission_mode` (confirmed working)
- `/init`, `/import [codex|gemini]`, `/memory`
- `/hooks`, `/insights`, `/recap`, `/release-notes`
- `/reload-plugins`, `/reload-skills`
- `/rate-limit-options`, `/sandbox`
- `/schedule [description]` (alias `/routines`) — cloud-side routines; may not map to komet's own scheduling story
- `/agents` — as of v2.1.198 just prints a reminder to ask Claude directly; low value as a dedicated command

Bundled skills (already prompt-shaped by design, so these should just work once discovery surfaces them — and per probe finding #2, `get_context_usage`'s `skillFrontmatter` with `source == "built-in"` is exactly how to discover them reliably):
`/batch`, `/code-review` (alias `/review`), `/security-review`, `/doctor` (alias `/checkup`), `/debug`, `/run`, `/run-skill-generator`, `/verify`, `/loop` (alias `/proactive`), `/claude-api`, `/dataviz`, `/design-sync`, `/fewer-permission-prompts`, `/simplify`, `/deep-research` (workflow, not skill, same shape).

## Bucket B — TUI-only / interactive dialogs / no server-side meaning (skip)

`/vim`, `/theme`, `/color`, `/scroll-speed`, `/keybindings`, `/focus`, `/ide`, `/desktop` (alias `/app`), `/radio`, `/powerup`, `/mobile` (aliases `/ios`, `/android`), `/passes`, `/heapdump`, `/login`, `/logout` (komet manages its own account swap), `/chrome`, `/install-github-app`, `/install-slack-app`, `/privacy-settings`, `/remote-control` (alias `/rc`), `/remote-env`, `/teleport`, `/list-agents` (alias `/peers`), `/exit` (alias `/quit`), `/help` (komet has its own UI for this), `/bug` (alias `/share`), `/feedback`, `/artifacts`, `/auto-mode-setup`, `/autocompact`, `/autofix-pr`, `/fast`, `/advisor`, `/design-login`, `/copy` (clipboard — meaningless server-side; komet already has its own copy affordance).

## Bucket C — dedicated control_request verbs (bypass the text-command channel entirely)

See the confirmed-shapes table under "Live probe results" above. These remain the **highest-value first targets**: no prompt-injection ambiguity, and (for the four confirmed ones) no guessing at output frame shape anymore. They don't fit the existing `SlashCommand` proto type at all (that type is "typed at the start of the composer, sent as prompt text" — these are host-side actions with structured request/response, closer in shape to `RunControls::request_permission` than to a `SlashCommand`).

## Open design question this inventory surfaces

Bucket A/B entries are genuinely "type `/name`, it's a prompt line" — they fit the existing `SlashCommand` → composer `/` popup path once discovery is fixed (see probe finding #2: switch to `get_context_usage`'s `skillFrontmatter`, or find why `initialize` returns empty). Bucket C entries are not slash commands to komet at all; they're **actions** (a settings toggle, a checkpoint picker, an MCP server list) that happen to also have a `/name` alias in the original CLI. Modeling them as new proto types (e.g. `HarnessAction` alongside `SlashCommand`) rather than shoehorning them into the composer's slash popup is probably right, but that's a decision for the next session, not this inventory.

## Suggested next steps

1. ~~Spawn the CLI in stream-json mode and diff `initialize`'s commands against reality.~~ **Done** — it's empty; use `get_context_usage`'s `skillFrontmatter` instead (probe finding #2).
2. ~~Send a raw `/compact` and `/clear` and capture stdout frames.~~ **Done for `/clear`** (fixed the normalizer gap it exposed); **redo for `/compact`** with a non-trivial conversation already in context to see the real `compact_boundary` shape, if any.
3. Fix `ClaudeHarness::discover_commands()` to read from `get_context_usage` (filtered to `source == "built-in"` for built-ins, `"userSettings"` for the custom/project ones the popup already wants) instead of — or in addition to — the empty `initialize` response. This is now the top priority: today the composer's `/` popup gets nothing from Claude Code.
4. Retest `mcp_reconnect` (with a real connected server name, and hunt for a no-arg "list/status" subtype) and `rewind_files` (find what enables it) — both errored on first guess.
5. Retest `stop_task` against a real spawned background task id to see a genuine stop, not just the bare-success no-op it gave for a fake id.
6. Only after 3–5: decide the proto/UI shape (extend `SlashCommand` for the fixed discovery source, add a new `HarnessAction` type + UI surface for Bucket C).
