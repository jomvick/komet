# Parity checklist

Status of the native rewrite against `docs/research/feature-inventory.md`
(§1–§8), audited against the tree after M6. Legend: **done** (implemented and
tested), **partial** (core in place, listed gaps), **deferred** (intentionally
not built yet).

## §1 Desktop app

| Item | Status | Notes |
| --- | --- | --- |
| 1.1 Window shell | partial | gpui window, always-dark theme, external links via OS browser. Deferred: frameless-inset/traffic-light chrome (macOS packaging not executed), single-instance lock, dev-vs-packaged port split (env vars instead). |
| 1.2 App phases | done | Gate / OrgGate ("Create your workspace" + memberships) / app with crossfade; boot splash with fade-out cap (`ui/src/shell.rs`). |
| 1.3 Shell layout | done | Collapsible drag-resizable sidebar (208–400), right Changes pane (360–760, 52% cap), header variants, widths persisted to `ui-settings.json`. |
| 1.4 Keyboard shortcuts | done | Customizable keymap, click-to-record with conflict detection, per-row reset (`ui/src/settings/shortcuts.rs`); persisted with UI settings. |
| 1.5 Routes | partial | Native navigation instead of URL routes; settings pages: devices / agents (harnesses) / accounts / appearance / files / notifications / shortcuts / archived / sync / security. Profile page (heatmap) is an §8 exclusion. |
| 1.6 Sidebar | done | Device switcher, new session, grouped-by-project or flat, status dots (staleness-checked), row context menu (rename/archive/delete), resort glide. |
| 1.7 Composer | done | Send/Steer/Stop morph, compact↔expanded flip, per-chat drafts, optimistic echo with failure return-to-draft, QuestionPanel (paged, auto-advance, number keys), all four pickers (harness/model, traits, repo with folder browser + clone/create, branch with worktree toggle), image attachments (paste/drop/picker → strip → chunked upload to host device → `withAttachments` refs in prompt text + inline image blocks for the Claude harness; per-chat stash, failure hand-back, lightbox — `ui/src/attachments.rs`). |
| 1.8 Transcript | done | Doc-projection source, virtualized, markdown + syntax highlight, tool folding (ToolGroup/ToolChip), input/error chips, stick-to-bottom band, MessageRail minimap (hover preview, hidden < 48rem), user-bubble attachment thumbnails (112×80, read-back from owning device, 2s→15s retry ladder, seeded cache, click-to-expand lightbox). |
| 1.9 Accounts settings | done | Provider cards, usage meters with 80/95% thresholds + reset time, Switch/Forget, paste-code and browser-poll add flows, device switcher (`targetDeviceId`). |
| 1.10 Terminal panel | done | Session-scoped tabs, drag-reorder, middle-click close, height drag, replay-then-tail streams, input coalescing, ANSI emulator (`ui/src/terminal/`). |
| 1.11 Changes viewer | done | Patch → file/hunk/line rows, per-file collapse, ±gutters, time-sliced highlighting, preparing/clean/error states, checkout_id → device+cwd resolution. |
| 1.12 Motion catalog | partial | Motion kit (cubic-bezier curves, fade-in/quick, splash-out, pulse/gradient spinners, menu/dialog-in, resort glide). Gap: prefers-reduced-motion switch. |
| 1.13 State & connection | done | All subscriptions (AuthStatus, WatchDevices/Chats/Sessions/CheckoutDiffs, per-chat WatchDocMessages, LocalDevice probe); reconnect from scratch. |

## §2 Control plane

| Item | Status | Notes |
| --- | --- | --- |
| ListHarnesses / ListModels | done | Relay-forwardable. |
| Run/Subscribe/Interrupt/Steer/RespondInput RPCs | done (changed shape) | Deliberate redesign: these ride the durable doc command queue (`QueueCommand {run|steer|interrupt|respondInput}`) instead of device-addressed RPCs — same capability, offline-tolerant. |
| Repos/folders/worktrees RPCs | done | All eight methods, relay-forwardable. |
| Uploads / ReadAttachmentChunk | done | Chunked staging → durable file; path-jailed reads; attachments live only on the host device. |
| Terminals RPCs | done | Open/Subscribe/Write/Resize/Close, forwardable. |
| Agent-account RPCs | done | Full login/activate/forget/poll surface, forwardable. |
| LocalDevice | done | `{deviceId}`; IPC-only (never forwarded). |
| DataRpc watches + QueueCommand | done | — |
| Mutate ops | done | createChat/renameChat/setChatArchived/deleteChat/renameDevice done; markChatSeen accepted as a no-op (unseen markers UI-local); `SetChatConfig` exposed as a Mutate op dispatching to `set_chat_config` (`engine/src/rpc.rs`). |
| AuthRpc | done (changed shape) | Deliberate redesign: local-first auth — dev bearer + self-hosted `komet-sync-server` (`docs/self-hosted-sync.md`). The legacy edge WorkOS stack (JWKS verify, `/auth/*` routes, org list/create at edge) was removed with `edge/` (see `ARCHITECTURE.md`); SignIn/SignOut/ListOrgs surface retained in dev/local shape. |
| Wire types | done | `komet-proto`: AgentEvent, ToolCall kinds, models/options, entities, AuthState. |

## §3 Backend engine

| Item | Status | Notes |
| --- | --- | --- |
| 3.1 Lifecycle | partial | Device registration, presence heartbeat (ephemeral, 15s), stale-session recovery, host-only doc executor with steer→new-turn fallback, single-instance data-dir lock, crash shield (process-wide panic hook: logs payload + location, leaves a `last-crash.log` marker reported and cleared at next boot; journal stamping stays with boot-time `recover_stale`). CLI auth decoupled from the daemon: `komet login`/`logout`/`status` work on the persisted session and exit; headless TTY sign-in remains, and off-TTY (systemd/launchd) headless fails fast with "run `komet login` first"; `komet daemon install/start/stop/restart/status/uninstall` manages launchd / systemd `--user` units (install-time PATH captured into the unit for harness CLIs). Gaps: login-shell PATH capture for the headed app, parent-PID watchdog. |
| 3.2 Sessions engine | done (one intentional deferral) | Run journal on disk with crash recovery (aborted stamps), steering mailbox at step boundaries, doc hooks at boundaries, streamed part folding at STREAM_COMMIT_MS, idle reaper (parked sessions end clean as `Idle` after 30min, no aborted stamp). Deliberately not ported: per-turn stall watchdog — a live child IS the working signal (`engine/src/sessions.rs`). |
| 3.3 Session-docs host | done | docs.sqlite snapshots + processed-command ledger, mark-BEFORE-execute, room join per open chat, diff sidecar publish, cold-chat delivery both directions (nudge POST on queue for remote-hosted chats + warm-open on nudge receipt). Gap (minor): no boot-time warm-open of recent chats (14d/30) — cold chats rely on nudges. |
| 3.4 Terminals | done | PTYs, 1MB bounded replay + `afterSeq` resume, 32 max, exited 30-min TTL, live shells survive detach. |
| 3.5 Repos/diffs | done | list/add/clone/create, branches, worktrees, checkout identity; CheckoutDiffSync (fs watchers + repair pass, name-status+numstat+patch incl. untracked, 3MiB cap, sha256, sidecar publish); chat.branch upkeep from HEAD watch; folder listing with timeout. |
| 3.7 Auth / uploads / accounts / device-room | done | Dev-bearer + self-hosted sync auth, org gate, dev mode; chunked uploads; claude/codex credential swap with usage probes and OAuth flows; host relay (virtual sockets over `{s,k,to,from}` frames) + peer link cache. The legacy WorkOS code+loopback / paste-code refresh flows were removed with `edge/` (see `ARCHITECTURE.md`). |

## §4 Harness

| Item | Status | Notes |
| --- | --- | --- |
| Claude Code adapter | done | stream-json, model discovery/effort ladders, AskUserQuestion → requestInput, steering via persistent input, init dedup, subagent filtering. **Live-verified against the real `claude` CLI 2.1.215**: doc-queued run → host executor → subprocess → streamed reply landed complete in the doc. |
| Codex adapter | done | `codex app-server` JSON-RPC (thread/start/resume, sandbox policy). |
| Cursor (ACP) | done | Shared `AcpHarness` spec; `cursor-agent acp` (Cursor's native ACP server), turn-boundary steering, no effort ladder (effort rides the model id's bracket suffix). Cursor's blocking extension methods are answered: `cursor/ask_question` → requestInput, `cursor/create_plan` auto-accepts, `cursor/update_todos` → todo chip. **Live-verified against the real `cursor-agent` CLI**: model discovery, streamed reply, todo chip and exec tool calls. |
| Sandbox / permissions (Paseo parity) | done (gaps) | Provider-native `SandboxOptions` (Codex/Claude/OpenCode, serde-strict) + `validate_run_request` fail-fast + options-win-over-level precedence + OpenCode `OPENCODE_CONFIG_CONTENT` overlay with merge semantics live-verified (`docs/superpowers/plans/2026-08-27-paseo-parity-sandbox.md`; `engine/tests/sandbox_validation.rs`). Non-sandbox providers reject non-empty options. Known gaps (OpenCode): no runtime check that `ask` patterns actually prompt through ACP; `read` defaults to `allow` with no sensitive-file deny list; no OS-level isolation (upstream sandbox-runtime plugin not integrated) — see `2026-08-28-sandbox-access-improvement-plan.md` tracks C1–C5. |
| Grok (ACP) | done | Shared `AcpHarness` spec; `grok agent stdio`, turn-boundary steering. |
| Hermes (ACP) | done | Shared `AcpHarness` spec; `hermes acp` (Nous Research's native ACP server), turn-boundary steering, no effort ladder yet. |
| Pi (ACP) | done | Shared `AcpHarness` spec; community `pi-acp` adapter (pinned 0.0.33, npx fallback), turn-boundary steering, minimal→max thinking ladder. |
| Mock harness | done | Scripted event replay; powers tests + the e2e smoke. |

## §5 Session doc schema

| Item | Status | Notes |
| --- | --- | --- |
| Containers (meta/messages/commands), LoroText bodies | done | Shape-compatible with TS `packages/session-doc`; `tokens` dropped per §8. |
| Command rules (append-only, host outcome writer, evaluateCommand) | done | Processed-ledger dedupe, TTL, supersede rules. |
| Continuation splitting / joining (MSG_INLINE_MAX 256KB) | done | `split at part boundaries`, `root#cN`, render-time join. |
| Render-parts privacy policy | done | WriteFile content / Edit bodies / etc. stripped; full inputs only in the host journal. |
| Sidecars (tail, diff) + constants | done | — |

## §6 Sync (self-hosted)

The legacy `edge/` Cloudflare Worker + Durable Objects + WorkOS + R2 stack has
been removed (`ARCHITECTURE.md`; `docs/self-hosted-sync.md`). Sync is Loro CRDT
docs through the self-hosted `komet-sync-server` (SQLite rooms, FS blobs,
bearer auth). The §6/§7 rows below describe the original inventory surface, not
the current backend.

| Item | Status | Notes |
| --- | --- | --- |
| Sync rooms (sessions/devices) | done | Loro rooms via `komet-sync-server`; `KOMET_EDGE_URL` + `KOMET_SYNC_TOKEN` seams. |
| Auth at sync | done | Bearer shared secret; dev mode `user@org` bearers. |
| Durable nudges / sidecars | done | Replay on join (cap), sidecar slots — reimplemented sync-side. |

## §7 Server → sync

| Item | Status | Notes |
| --- | --- | --- |
| WorkOS exchange/refresh, org list/create at edge | dropped | Removed with `edge/`; org gate is local-first now. |
| Postgres/signaling dropped | done | Nothing depends on them. |

## §8 Exclusions

| Item | Status | Notes |
| --- | --- | --- |
| Token-usage display dropped | done | No WatchUsage, no doc `tokens`, no profile heatmap; rate-limit meters + Usage AgentEvent passthrough kept as specified. |

## Deferred (cross-cutting)

- **Mobile app** — out of scope for the native rewrite so far.
- **E2EE** — transport is TLS + sync bearer tokens; end-to-end encryption of doc
  contents not designed.
- **macOS packaging execution** — config + steps in `dist/` only (needs a Mac).
- **Engine hardening**: parent-PID watchdog, idle reaper / stall watchdog, boot warm-open of recent chats.
  (Single-instance data-dir lock and crash shield are done — see 3.1; per-turn
  stall watchdog is deliberately not ported — see 3.2.)

## Summary

Table rows above: **41 done · 4 partial**, plus the cross-cutting deferrals
(mobile, E2EE, macOS packaging execution, engine hardening) — the last
overlaps the named gaps in the partial rows.
