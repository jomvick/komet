# komet — Architecture

> Fork of [zeronsh/comet](https://github.com/zeronsh/comet) — MIT, Copyright Wing. See `LICENSE` and `THIRD_PARTY_NOTICES.md`.

A native controller for coding agents (Claude Code, Codex, Cursor, Grok, Hermes, OpenCode, Pi) —
Rust engine + gpui UI, single binary. **100% local by default**: no account, no login
screen, no network calls. Multi-device sync is built (Loro CRDT docs through the self-hosted
`komet-sync` server) but **disabled by default** — see "Multi-device sync" in the README.

**Pillars:**
- Local-first: every device runs a small engine that stores sessions on that device; the same
  Loro CRDT docs persist locally when sync is disabled.
- Optional sync uses Loro CRDT docs (loro-mirror model) through the self-hosted `komet-sync`
  server (`crates/sync-server`: Rust + SQLite rooms + FS blobs, bearer-token auth).
  The legacy `edge/` Cloudflare Worker + WorkOS stack has been removed.
- Everything device-side is Rust; no TypeScript services remain.
- Token-usage display is excluded (poor fit for CRDTs).
- Frontend is **gpui** (pinned Zed rev). Virtualization + markdown techniques ported from
  **mugen + pretext** (`docs/research/mugen-pretext.md`).
- One binary, **headed or headless**. Smooth transitions/animations
  (catalog in `docs/research/feature-inventory.md` §1.12).

## 1. Topology (unchanged shape, new materials)

```
gpui UI ─ in-proc/localhost RPC ─ engine A ══ device-room relay ══ engine B ─ RPC ─ gpui UI
                     │     optional komet-sync server: rooms, blobs, bearer auth    │
                     └── optional Loro sync ─ session/registry rooms (per chat) ────┘
                                           └─ Workspace registry room ─────────────┘
```

- **Engine = backend** (was `@komet/backend`): runs agents, owns auth, terminals, repos/worktrees,
  diff sync, doc hosting. Pure Rust daemon, fully functional headless.
- **UI = viewport** (was Electron): gpui app rendering engine state. Talks the same typed RPC whether the engine is in-process or a separate daemon. Organized around **spaces** — (device, folder) pairs, local or synced according to the active profile. The sidebar is the data: an attention-sorted Sessions list, filtered by a searchable spaces dropdown ("All spaces" included) that also hosts space management. The horizontal tabs are a **device-local viewport** onto that list (`ui-settings.json` `openTabs`, cross-space): closing a tab is local-only — archiving is an explicit sidebar action — and a sidebar click (re)opens a session as a tab. The new-session canvas carries a space picker (defaulting to the sidebar filter, else the last selected space); new sessions are minted onto the picked space's device via relay-forwardable RPCs.
- **Sync server (`komet-sync-server`)**: self-hosted Rust service (VPS via Docker or any PC).
  SQLite per room (frames), blobs on the filesystem, shared-secret Bearer auth
  (`KOMET_SYNC_TOKEN`). No accounts, no third-party identity provider.

### Headed / headless
Single binary `komet`:
- `komet` — headed. If a local engine daemon is already listening on the IPC port, connect to it;
  otherwise run the engine **in-process** (RPC over an in-memory duplex — same protocol, zero
  serialization shortcuts, so the boundary stays honest) **and serve that same engine on the IPC
  port**. The embedded engine is not private: any other viewport can attach to the running app
  without it first being restarted as a daemon. Binding is best-effort — if the port is taken the
  window still opens, having lost only the ability to host peers.
- `komet headless` — engine only. A clean installation immediately serves its local profile over localhost IPC; when a saved account selects the synced profile at startup and a bearer is available, it also hosts its DeviceRoom for remote control. A VPS can run this while a laptop's UI drives it.

### Local-first workspace profiles

Authentication and workspace selection are deliberately separate state machines:

- `AuthState` is live credential state: `SignedOut`, `NeedsOrganization`, or `SignedIn`. It may change after login, refresh, revocation, or logout.
- `WorkspaceScope` is the immutable storage and transport boundary captured once at engine startup: `Local`, `Synced`, or explicit `Development`.

The engine never re-resolves an open store because `AuthState` changed. This prevents a sign-in, token refresh, or revocation from silently swapping databases or attaching online transports to a runtime that started local-only.

| Startup condition | `WorkspaceScope` | Online transports |
| --- | --- | --- |
| Sync server configured, no parseable saved `session.json` | `Local` | Disabled |
| Parseable saved session | `Synced` | Enabled when a bearer is available |
| No sync server configured and no dev bearer | `Development` | Disabled |
| Explicit non-empty dev bearer | `Development` | Enabled |

`komet login` and `komet logout` operate on `session.json` while the engine is stopped. Login selects `Synced` for the next start; logout selects `Local` for the next start. The UI may update live authentication status, but the active `WorkspaceScope` still changes only after restart.

**Current default (local-only):** no sync server ships preconfigured (`KOMET_EDGE_URL` unset →
`None`), so every start resolves to the "Development" row —
scope, no account, no login screen, zero network calls. To enable the synced profile later:
deploy `komet-sync-server` (see `docs/self-hosted-sync.md`), then set `KOMET_EDGE_URL`
and `KOMET_SYNC_TOKEN` on each device; the startup table above applies as written.

The resolved profile selects the session snapshots, registry snapshot, run journals, and attachment cache that may contain workspace data:

| Scope | Store and journals | Uploads |
| --- | --- | --- |
| `Local` | `{data_dir}/profiles/local/` | `{data_dir}/profiles/local/uploads/` |
| `Synced` | `{data_dir}/orgs/{org_id}/{user_id}/` | `{data_dir}/orgs/{org_id}/{user_id}/uploads/` |
| `Development` | `{data_dir}/orgs/{org_id}/{user_id}/` | `{data_dir}/orgs/{org_id}/{user_id}/uploads/` |

The synced and development store roots preserve the historical cloud layout while their attachment caches are account-scoped. Local identity lives in `{data_dir}/local-profile.json`; its UUID is stable across restarts and is not an account or development identity.

Older releases wrote every synced and development attachment to `{data_dir}/uploads/`, and persisted those absolute paths in transcripts. On upgrade, the first synced or development account that opens this legacy cache claims it in `{data_dir}/legacy-uploads-owner.json`. That account may read the cache as a compatibility fallback, but all new staging and commits use its account-scoped uploads root; other accounts cannot read or write the legacy cache.

Device identity and machine resources remain device-scoped under the common data directory: `device-id`, repository registration, managed worktrees, agent credentials/accounts, and UI settings. They are available across profiles, but they do not contain or expose another profile's transcripts or attachments.

#### Privacy boundary and follow-ups

This first local-first change does not upload, import, link, or delete local sessions when a user signs in. Local attachments remain jailed under the local upload root and are not readable through the synced attachment cache. Returning to local-only mode reopens the same local identity and data.

The following product work is intentionally deferred:

1. Explicit session selection and copy between local and synced profiles, including attachment copying, provenance, and conflict behavior.
2. Browsing both scopes simultaneously or switching the visible scope without restarting the engine.
3. A supported self-hosted backend contract covering authentication modes, room APIs, authorization, persistence, and blob storage. Current endpoint and bearer overrides remain development/deployment seams, not a promised compatibility surface.

## 2. Data model — all Loro

Two persistent doc kinds. When sync is enabled they share one loro-protocol room protocol over WebSocket; local-only profiles persist the same docs without joining rooms:

1. **Session doc** (per chat) — the transcript + durable command queue. Schema is a Rust port of
   `packages/session-doc` (same container names/shapes so the sync tail materializer keeps working): `meta` map, `messages` list (parts as list-of-maps with **LoroText bodies** — the
   measured 1.03× oplog shape; never LWW value rewrites), `commands` list with ledger rules 1–3
   (append-only per-device entries; host-only outcomes; dedupe/TTL/supersede evaluation).
   Continuation splitting at 256KB, render-only tool parts (full inputs stay in the host's local
   run journal), tail/diff sidecars. Constants carried over (`STREAM_COMMIT_MS=120`,
   `DO_FLUSH_MS=5s`, compaction at 8MB, retain 30d, tail 64).

2. **Workspace registry doc** (per profile) — the `registry1` snapshot stores spaces (id, deviceId, path, name?, gitDetected, checkoutId), the chats index (id, deviceId, title, archived, cwd, branch, checkoutId, spaceId, lastSeenAt, lastMessagePreview/At, config), devices, session-status rows, and checkout-diff summary pointers. A space is a device+folder pair in the active profile; the owning device's `SpacesSync` stamps git presence so branch pickers and the diff sidebar can gate without another RPC. Local scope keeps the registry entirely in its profile store. Synced and development scopes join `/registry/{orgId}/ws`, backed by the private per-user room `reg1/{orgId}/{userId}`; rows are never visible to every member of an organization.

   Writer discipline: each device writes its own device and session-status rows, rows for chats it hosts, and git stamps for spaces it owns. Creates, renames, archives, and seen marks are LWW sets accepted from any device. `deleteSpace` tombstones the space and every chat/session row in it in one commit. Presence uses ephemeral room frames rather than durable heartbeat writes.

   *Why one registry and not N tiny docs:* the sidebar needs one subscription for the whole list (grouping, resort animations, unseen markers). Its rows contain indexes rather than transcripts, so one local snapshot and, when enabled, one room connection remain bounded and cheap.

3. **Mirror layer** (`komet-doc` crate) — Rust equivalent of loro-mirror: typed structs for the
   schema, **incremental** application of `doc.subscribe` diffs into cached state (no full
   re-hydration per change — this is also what fixes komet's known O(transcript) re-projection
   inefficiency, remaining-work item 1a), and a diff-reconcile write path (evaluate `lorosurgeon`
   0.2.x as a dep; our schema is small enough to hand-roll if it doesn't fit). The UI renders
   mirror state directly with per-entry change notifications — the "endgame" the TS
   implementation documented but never reached.

### Command plane
Send/steer/interrupt/respondInput = durable command entries in the session doc (`QueueCommand`),
executed by the chat's **host** device (executor gated on chat ownership; mark-processed BEFORE
execute; steer with no live run dispatches as the next turn). Offline sends queue in the doc.
This is komet's proven design, kept verbatim.

## 3. Cargo workspace

```
komet/
  Cargo.toml                 # workspace
  crates/
    proto/        komet-proto    # wire types: AgentEvent, ToolCall, RunRequest, Model,
                                 # entities, RPC envelopes (serde; ndjson framing);
                                 # `view` = the pure derivations both frontends share
                                 # (sort orders, staleness gating, grouping, boot gate)
    doc/          komet-doc      # session-doc + workspace-registry schemas, mirror layer,
                                 # parts fold, continuations, command ledger, sidecars
    sync/         komet-sync     # loro room client (join/VV backfill/fragments/backoff),
                                 # ephemeral presence, DocsStore (SQLite snapshots +
                                 # processed-command ledger)
    harness/      komet-harness  # Harness trait over the ACP protocol (claude/codex/cursor/
                                 # grok/hermes/pi/opencode via org-maintained
                                 # adapters + managed npm install), mock; steering mailbox,
                                 # requestInput, models/reasoning/options catalogs
    engine/       komet-engine   # sessions engine (pub/sub, run journal, recovery, stall
                                 # watchdog), doc host + command executor, repos/worktrees,
                                 # checkout-diff sync, terminals (portable-pty), uploads,
                                 # agent accounts (cred swap), auth (bearer via sync server),
                                 # device-room host/peers, identity, single-instance lock
    rpc/          komet-rpc      # UiRpc/ControlRpc: typed req/resp/stream over WS (tokio-
                                 # tungstenite) + in-memory transport; device-room virtual
                                 # sockets ({s,k,to,from} frames)
    ui/           komet-ui       # gpui app: shell, sidebar, conversation, composer,
                                 # terminal view, diff pane, settings, animation kit
    syntax/       komet-syntax   # tree-sitter syntax highlighting contracts (paint-only
                                 # token runs; no UI/RPC/engine deps)
     update/       komet-update   # self-update: versioned dirs + `current` symlink, service
                                  # restart, macOS app-bundle staging (macOS + Linux only)
     sync-server/  komet-sync-server # self-hosted sync server (SQLite rooms, FS blobs, bearer auth)
   apps/
     komet/                       # the binary (headed default, `headless` subcommand)
   docs/                          # this file + research reports
```

Engine async runtime: **tokio** throughout; the UI bridges via `gpui_tokio` (`Tokio::spawn`
futures surfaced as gpui `Task`s). In-process mode runs the engine on its own tokio runtime
thread; the UI never blocks on it.

## 4. UI plan (gpui) — parity + smoothness

Reference: `docs/research/gpui.md`, `docs/research/mugen-pretext.md`,
feature spec `docs/research/feature-inventory.md` §1.

- **Deps**: `gpui` + `gpui_platform` pinned to one Zed rev (Apache-2.0). **We do not use Zed's
  GPL crates** (`markdown`, `ui`, `theme`, `editor`) — markdown, components, and theme are ours.
- **Transcript**: gpui `list()` + `ListState::new(n, ListAlignment::Bottom, overdraw)` (sum-tree
  offsets, follow-tail). On top of it, port the mugen behaviors that gpui doesn't give us:
  - stick-to-bottom **spring** with feed-forward tracking of streaming growth; interrupt from
    *user input* (wheel-up / drag), re-engage within a 70px band; own-send re-engages + smooth
    scrolls;
  - **block-granularity rows** (one row = one markdown block / tool group, not one message) with
    stable ids `msgId#blockId`; live turn stays unsplit, re-splits on persist; optimistic echo
    rows share the client-minted id so persistence never flickers;
  - row height memoization keyed by (row id, content length, width) so a streamed token
    re-measures one row;
  - scroll-anchor absorption for above-viewport height changes.
- **Markdown** (`komet-ui::markdown`): `pulldown-cmark` parsing on `background_spawn` with
  coalescing (Zed's proven pattern), block-level incremental re-parse of the streaming tail
  (incremark's O(delta) idea: only re-parse from the last stable block boundary), monochrome
  theme where **numbers drive layout, colors are paint**. Code blocks: monospace, no wrap ⇒
  height = lines × line-height (layout independent of highlight); syntax highlighting via
  `synoptic`/`syntect`-class tokenizer run time-sliced in the background, colors applied as text
  runs (paint-only). Streaming **fade-in veil** on newly appended text via `with_animation`
  opacity (paint-layer, never affects layout). `prefers-reduced-motion` honored.
- **Composer**: hand-rolled gpui text input (start from Zed's `examples/input.rs`: IME, selection,
  clipboard, key actions), compact↔expanded auto-flip by measured text width, auto-grow 76–260px,
  Enter/Shift+Enter, Send→Steer→Stop morph, drafts + attachments per chat, drag-drop/paste
  images, QuestionPanel (paged, 1-9 keys, 220ms auto-advance) replacing the composer while input
  is requested. Pickers (harness/model, traits, repo w/ folder browser, branch w/ worktree
  toggle) as gpui popovers with `menu-in` scale/fade.
- **Terminal**: `alacritty_terminal` (vte state machine, MIT/Apache) + `portable-pty` on the
  engine side; custom gpui grid element; tabs w/ drag-reorder (150ms sliding transforms), height
  drag 160px–55vh, 12ms input coalescing / 80ms resize debounce, 1MB replay, detach ≠ close.
- **Diff pane**: unified-patch parser → virtualized file/hunk/line rows, per-file collapse
  (180ms height tween), time-sliced highlight, 200ms width transition on the pane itself.
- **Animation kit** (`komet-ui::motion`): small helpers over gpui `Animation` reproducing the
  komet catalog — `fade-in` (0.5s, cubic-bezier(0.16,1,0.3,1), translateY 4→0), `splash-out`,
  `komet-pulse` staggered cell wave (boot splash + loaders), `gradient-spin-pulse` matrix
  spinner (WorkingIndicator + rotating flavour word), `menu-in`/`dialog-in` scale-fades, 200ms
  ease-out width/height transitions for sidebar/panes, sidebar-resort **slide animation**
  (we own the list, so animate row positions directly — the View Transitions equivalent, 260ms
  cubic-bezier(0.22,1,0.36,1)), reduced-motion switch.
- **Theme**: always-dark monochrome, oklch-derived neutral scale precomputed to Hsla, hairline
  borders, Geist/Geist Mono bundled fonts.

## 5. Engine plan

Direct ports of komet behaviors (spec: feature-inventory §3):
- **Sessions engine**: per-session broadcast hub; on-disk run journal (resumable `seq` replay,
  crash auto-resume); persistent steerable sessions (steering mailbox at step/turn boundary; idle
  reaper; 10min stall watchdog); recovery stamps `aborted`.
- **Doc host**: per-chat handle (join room, VV backfill, write user entries + stream assistant
  segments at 120ms commits, drain commands host-only with processed-ledger idempotence, publish
  diff sidecar, presence); warm-open recent chats (14d/cap 30); nudge-driven cold open; SQLite
  snapshot store.
- **Harness** (`docs/research/harness.md`, protocol decision `docs/research/acp.md`): every
  agent speaks the **ACP protocol** through an org-maintained adapter (`claude-agent-acp`,
  `codex-acp`, `pi-acp`, …) — one shared `AcpHarness` with per-agent
  specs (executable, npm package, model/reasoning/option catalogs, steering mode, quiet-settle
  behavior). Adapters install once via npm, lazily on first use (`adapter_install.rs`), or
  resolve from PATH / npm global bins / login-shell PATH (`shell_env.rs`); a `mock` harness
  covers tests. The bespoke stream-json (Claude) and app-server JSON-RPC (Codex) harnesses
  were retired with the ACP conversion — those modules now hold static model catalogs.
- **Repos/diffs**: git2 or `git` subprocess (subprocess — matches komet, avoids libgit2 edge
  cases); worktrees under `~/.komet/worktrees`; fs watchers (`notify`) + 2min repair; diff
  capture (patch + numstat + untracked, 3MiB cap, sha256) → workspace registry summary + diff
  sidecar.
- **Agent accounts**: credential-slot swap (macOS Keychain via `security-framework`, files
  elsewhere), plan labels, usage probes, paste-code/browser-poll OAuth flows.
- **Auth**: bearer shared secret (`KOMET_SYNC_TOKEN`) against the self-hosted `komet-sync`
  server; no third-party identity provider, no login screen. Default is **no sync server
  configured** ⇒ pure local scope; setting `KOMET_EDGE_URL` + `KOMET_SYNC_TOKEN`
  enables the synced profile.

## 7. Parity exclusions & deliberate changes

- **Excluded**: token-usage display (profile heatmap, lifetime stats, per-message token columns,
  `WatchUsage`). Rate-limit meters on agent accounts are *kept* (separate concern; probed from
  CLIs, not CRDT-synced).
- **Changed**: Postgres entity sync/server → workspace registry + self-hosted sync server; Electron/React/mugen → gpui with
  ported techniques; Node harness SDKs → ACP adapter protocols; WebRTC → device-room relay.
- **Kept verbatim**: session-doc schema shape + constants, command ledger rules, room design,
  render-parts privacy policy, UX behaviors and animation timings.

## 8. Milestones

Status legend: ✅ shipped · 🟡 shipped with named gaps (see `docs/PARITY.md`).

- ✅ **M0 Scaffold** — workspace builds; `proto`/`doc` crates with ledger + parts + continuation
  unit tests; gpui hello-window runs.
- ✅ **M1 Doc + sync core** — `komet-doc` mirror over loro 1.13; room client syncs with the
  self-hosted sync server; Rust⇄server⇄Rust convergence test (M1 exit: two Rust peers converge
  through a real session room, tail endpoint serves).
- ✅ **M2 Engine core** — Claude harness end-to-end headless: `komet headless` + dev auth runs a
  turn, journal + doc writes, recovery test.
- ✅ **M3 UI core** — shell (sidebar/panes/header), transcript (virtualized, markdown, streaming,
  stick-to-bottom), composer (send/steer/stop, question panel); local chat fully usable headed.
- ✅ **M4 Multi-device** — device-room host/client virtual sockets, remote device control, workspace
  registry sync, bearer auth, presence. Proven live by `scripts/e2e-smoke.sh`:
  two headless engines against a real sync server — B queues a run into the chat doc, the
  nudge wakes host A, A executes (mock harness), transcript + session status sync back to B.
- 🟡 **M5 Full surface** — terminals, diff pane, repo/branch/folder pickers + worktrees,
  agent accounts UI, settings (devices/shortcuts/archived), Codex + Cursor + Grok + Hermes +
  OpenCode + Pi harnesses. Gaps: composer attachment UI (engine upload RPCs exist).
- 🟡 **M6 Polish** — wire reconciliation (proto AuthState on the wire, `LocalDevice`),
  two-device e2e smoke, keyboard map, clippy/fmt sweep, Linux packaging
  (`scripts/package-linux.sh` + release profile), macOS bundling config (`dist/macos/`,
  not executed — needs a Mac). Gaps: Windows packaging —
  the workspace already cross-compiles for `x86_64-pc-windows-gnu`
  (UI included), but daemon, updater, credential ACLs and packaging are still unix-oriented.

## 9. Open questions (tracked, non-blocking)

1. ~~loro-protocol Rust client ⇄ TS edge interop~~ — resolved by the Rust sync server
   (the frame protocol is small and we control both ends).
2. `lorosurgeon` fit for the mirror write path vs hand-rolled reconcile.
3. Cursor harness (komet has it; CLI surface for Rust TBD) — parity item, scheduled after Codex.
4. Text shaping performance for analytic row heights: gpui measures shaped text natively (Rust ⇒
   cheap), so we start with gpui `list()` measurement + memoization rather than porting pretext's
   full analytic kernel; revisit only if cold-open of huge transcripts measures slow.
