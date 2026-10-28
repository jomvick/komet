# Environments — separate workspaces, remote VPS control — design

Date: 2026-10-27
Status: Awaiting approval (brainstorming). Approach A chosen by the user.

## Summary

Add **environments**: a named, isolated workspace backed by **its own sync server**.
A VPS runs one command, prints a pairing link; the desktop app pastes that link and
gets a second entry in a sidebar switcher. Sessions started in that environment run
on the VPS. The two sides never share a registry, a room, a token, or a sidebar list.

The user's words: *"les sessions du VPS ne doivent pas être mélangées aux tiennes
dans la même sidebar, comme ça chaque partie est séparée et on sentira ça dans l'UI
aussi."*

## Why now

M4 shipped the hard part: the device-room relay (`crates/rpc/src/device_room.rs`),
`targetDeviceId` RPC forwarding (`crates/engine/tests/device_routing.rs`), and minting
sessions onto a remote device's space. What blocks the user's scenario is onboarding
and scoping, not transport:

1. No `komet pair` — you set `KOMET_EDGE_URL` + `KOMET_SYNC_TOKEN` by hand on every
   machine, then restart.
2. No "Add environment" surface in the UI.
3. `edge` is **global to the engine**: `DocHostConfig.edge: Option<EdgeConfig>`
   (`crates/engine/src/doc_host.rs:164`), `WorkspaceHostConfig.edge`
   (`crates/engine/src/workspace_host.rs:138`). One server, one registry, one token.
4. One shared bearer token for the whole server, never revocable.

## Verified facts the design depends on

- Registry room = `/registry/{orgId}/ws` → `reg1/{orgId}/{userId}`; chats/spaces are
  rows keyed by that room, and the sidebar reads them as one stream
  (`WatchChats/WatchSpaces/WatchDevices/WatchSessions`, `crates/rpc/src/lib.rs`).
- Chat docs join `/session/{chatId}/ws` and `/chat2/{chatId}/ws` through the same
  `EdgeConfig`. A chat's transcript lives on exactly one sync server.
- The engine dials **outbound** to the sync server (`komet_sync::RegistryClient`,
  `DocHost` room clients). A headless VPS therefore needs **no inbound port** and no
  certificate of its own; only the sync server must be reachable over HTTPS.
- `komet sync-server` and `komet headless` are both library calls already
  (`komet_sync_server::serve`, `Engine::run`) — one process can host both.
- `komet daemon install` writes a systemd **user** unit (Linux) / launchd agent.
- The sync server has exactly one bearer check (`crates/sync-server/src/lib.rs:115`)
  and four room routes.

## Goals

- One command on a fresh VPS prints a pairing link; nothing else to type.
- Paste the link in the desktop app → the environment appears in a switcher and its
  sessions can be viewed and started from there.
- Sessions, terminals, git and files for that environment execute on the VPS.
- **No mixing**: the local sidebar never shows remote rows, remote search hits, or
  remote notifications; "All spaces" is per environment.
- Removing an environment stops all its connections and forgets the token, without
  touching the local workspace.
- The local workspace keeps working offline and unaffected when the VPS is down.

## Non-goals (named, not hidden)

- No mobile or web client in this plan. It is a separate client project; this design
  only leaves it a seam (a stable link format + an `/env` descriptor).
- No merging of environments, no cross-environment search, no moving sessions
  between environments, no shared inbox.
- No per-device or scoped tokens; the environment token remains a shared secret.
- No sync of the environment *list* between your own machines (each client adds it
  once). That is a separate, chicken-and-egg problem.
- No changes to the sync server's auth model or room naming.

## Anti-goals (would make this a failure even if it works)

- Rows from two environments visible in one list, ever, including transiently at
  switch time.
- A failed or slow remote blocking boot, or a removed environment resurrecting on
  restart.
- Printing the token to logs on every start.

## Approach chosen: A — one environment = one sync server

Each environment is a distinct sync server with its own token, rooms and registry.
Isolation is a network boundary, not a filter: there is no code path where the
laptop's registry and the VPS's registry meet.

Rejected alternatives:

- **B — one server, namespaced rooms + per-env tokens.** Cheaper to host, but needs
  real auth work (the server has a single constant-time bearer comparison, no
  scoping, no revocation) and isolation by convention across four room types.
- **C — UI connects directly to the remote engine over WSS.** Cleanest mental model,
  but requires an inbound port + certificate on the VPS and a multi-`RpcClient` UI
  refactor (every call site assumes one engine), and it loses offline queueing into
  the remote doc.

## Design

### Environment model

```rust
struct Environment {
    id: String,        // locally generated uuid; "local" is reserved
    label: String,     // "VPS ovh", defaults to the url host
    url: String,       // sync server base, e.g. https://sync.example.com
    token: String,     // shared bearer for that server
    namespace: Option<Namespace>, // {org_id, user_id} learned from GET /env
    added_at: i64,
}
```

The local workspace is the implicit environment `"local"`; it is never removable and
its record is not stored in the environments file.

### Engine: one runtime per environment

Device-scoped singletons stay single: `sessions`, `terminals`, `repos`,
`agent_accounts`, `uploads`, `workspace_files`, `device_id`, the instance lock.
They describe *this machine*, not a workspace.

Only the workspace-scoped pieces multiply:

```rust
struct EnvRuntime {
    id: String,
    edge: Option<EdgeConfig>,
    docs: Arc<DocsStore>,      // {data_dir}/environments/{id}/docs.db
    doc_host: DocHost,
    workspace: WorkspaceHost,  // its own registry doc + client
    links: Arc<LinkCache>,     // device routing *inside* that environment
}
```

`EngineCore` keeps `local: EnvRuntime` plus `envs: RwLock<IndexMap<String, Arc<EnvRuntime>>>`.
Building a runtime is the same code path as today's local bootstrap, parameterized by
a store root and an optional `EdgeConfig`; no new construction logic is invented.

Run journals, worktrees and agent credentials stay in the shared data dir: a chat id
is globally unique, and only the environment that hosts a chat ever executes it.

### Transport: one RPC connection per environment

The desktop UI keeps talking to **its own local engine** over IPC, but opens one
authenticated WebSocket per environment: `ws://127.0.0.1:{ipc_port}?env={id}`.
`serve_connection` resolves `env` once and dispatches to that `EnvRuntime`.

This avoids threading an `env` parameter through every method and every stream: the
existing per-connection code (`RpcClient`, `AppState` streams, `Watch*`) is reused
unchanged, one instance per environment. Routing to remote *devices* inside an
environment keeps using that environment's `LinkCache` — untouched.

Local-only methods (`EngineInfo`, `LocalDevice`, `ProbeSync`, updates, harness
management, `AddEnvironment`) are rejected on non-local connections: they describe the
machine, not the workspace.

> Implemented deviation: harness management (`ListHarnesses`, `GetHarnessCliStatus`,
> `ManageHarnessCli`, `SetHarnessEnabled`) is *not* local-only — pairing a remote
> machine is worth little if its agents cannot be enabled from here. On an environment
> connection these four demand a `targetDeviceId` (`environment_target_required`) and
> are forwarded to that device over the environment's own relay, so the answer always
> describes the machine that runs the agents and never the controller.

The transport choice must not become a special case: the in-process fallback creates one
in-memory connection per environment (`memory_client` already supports it), so `?env=`
works with or without an IPC port.

**Session stream scoping.** `WATCH_SESSIONS` today returns the device-wide
`SessionsEngine` map (`crates/engine/src/rpc.rs:1656`). On an environment connection it
is filtered to chats present in that environment's registry index — the registry is the
authority for "which chats exist here", and the run-state rows for chats the local
machine hosts belong to the local environment. Without this filter the remote sidebar
shows local chats, and the notification/sound path (`crates/ui/src/shell.rs:1470`) fires
for the wrong workspace.

**The laptop is not a device inside a remote environment.** It has no execution role
there: it never upserts a device row into that registry, never counts in that
environment's device list, and a space in a remote environment can never point at a
laptop path. Row-creating mutations (`createChat`, `createSpace`) are forwarded to a
device that belongs to that environment using the existing `targetDeviceId` routing, so
the VPS stays the only writer of its own registry. This is also what keeps `device_id`
single-valued and the doc-host ownership gate (`doc_host.rs:1480`) sound: two runtimes
can never both consider themselves the host of the same chat.

**Adding the server you are already synced to is rejected.** `AddEnvironment` compares
the `orgId`/`userId` returned by `GET /env` with the active profile's; a match is an
error (`AlreadySyncedHere`) rather than a second runtime over the same rooms. URL
comparison is not enough: `komet serve` dials its own loopback while advertising
`--public-url`.

### Persistence and lifecycle

- `{data_dir}/environments.json` — the environment list. Written atomically (temp +
  rename), mode 0600 because it holds tokens.
- `{data_dir}/environments/{id}/` — that environment's docs mirror (SQLite snapshots,
  processed-command ledgers). Never shared with another environment.
- The **local** environment keeps its current root untouched (`profiles/local/` or
  `orgs/{org}/{user}/` per the startup table). `EngineProfile` and `WorkspaceScope` are
  not re-resolved: an environment runtime gets its root handed in at construction, and
  the `orgId`/`userId` from `GET /env` is used only for room naming, never for the
  store root. Existing users keep their sessions on upgrade.
- `AddEnvironment { label?, url, token }` — validate by dialling `GET {url}/env` with
  the bearer (5 s timeout, clear error per case: DNS, TLS, 401, wrong server); persist;
  build the runtime; return it. Idempotent on `(url, token)`: returns the existing id.
- `RemoveEnvironment { id, purge }` — drop the runtime (close registry + room clients),
  remove the entry, and delete `environments/{id}/` only when `purge` is true. Default
  is forget-only: the mirror is the last copy if the server is gone, so deleting data
  is an explicit checkbox in the confirmation, never an implicit side effect.
- A `"local"` id in Add/Remove is rejected.

### Sync server: `GET /env`

New authenticated endpoint returning the server's workspace identity:

```json
{ "v": 1, "label": "komet VPS", "orgId": "...", "userId": "...", "version": "0.9.3" }
```

It exists so a client learns the room namespace (`reg1/{org}/{user}`) and the protocol
version in one round-trip, instead of stuffing identity into the pairing link. It
leaks nothing beyond what `reg1/` already implies, and requires the same bearer.

Identity comes from `KOMET_ORG_ID` / `KOMET_USER_ID` when set. When they are unset the
server generates both once and persists them in `{data_dir}/server-identity.json`
(0600), so the existing Docker `komet-sync-server` deployment pairs without new
configuration, and `komet serve` reuses the same file. No new auth: the endpoint is
behind the bearer the client already has.

### Pairing link

```
komet://pair/<base64url-nopad({"v":1,"label":"VPS ovh","url":"https://sync.example.com","token":"…"})>
```

- Printed by `komet serve --public-url https://…` on first run, and on demand with
  `--print-link`. Never printed by a plain service restart.
- Pasted into **Settings → Environments → Add environment**; the paste box also
  accepts the raw JSON for debugging. The UI previews `label` + host before adding.
- The link is a secret. It is not written to logs; `komet status` prints it masked.

### `komet serve` — the VPS entry point

```
komet serve [--public-url URL] [--port 8787] [--install-service] [--print-link]
```

One process runs both halves (`tokio::try_join!`): the sync server bound to
`127.0.0.1:{port}` and the engine pointed at it. `--public-url` is the HTTPS origin
your reverse proxy (Caddy) exposes; without it the command prints the token and the
exact Caddy snippet instead of a link, because a wrong URL in a link fails confusingly
later.

> Implemented deviations from this section:
> - `komet serve --lan` exists and *does* print a plain-HTTP link, on this machine's
>   private address. That was the actual ask (pair a VM on the home/office network
>   without first standing up TLS), so the rule became "TLS required on the internet,
>   opt-in on a network you control": `--lan` prints the warning next to the link, the
>   link format is unchanged, and the desktop refuses to pair a non-loopback `http://`
>   URL until the operator ticks the trusted-LAN box.
> - `install.sh --serve [--lan | --public-url URL]` is the one-command path for a
>   machine with no Komet yet: install, register the service, print the link.
> - The surface shipped as **Settings → Sync → Add device**, and the workspace picker
>   is the **Devices** menu at the top of the sidebar — the vocabulary the flow is
>   described in, not "Environments".

First run creates `{data_dir}/serve.json` (0600) with a generated token, a stable
`orgId`/`userId`, the port and the public URL, and exports them to the engine
(`KOMET_ORG_ID`, dev bearer, `KOMET_SYNC_TOKEN`) — reusing the existing development
scope instead of inventing a second identity system.

`--install-service` reuses the `daemon` module with a `serve` unit. On a VPS, a systemd
**user** unit needs `loginctl enable-linger $USER`; the command says so, and the docs
show the system-unit alternative.

### UI

- **Switcher**: the sidebar header's spaces dropdown becomes an environment list
  ("This computer", then each added environment with an online/offline dot), and the
  spaces filter moves one level down. "All spaces" always means *all spaces of the
  current environment*.
- **Every environment stays connected (decided).** Each added environment keeps its
  registry client, presence and relay link alive while you work anywhere else. So:
  the switcher's dot is live truth (registry/relay state, not a probe), a switch is a
  warm swap — no reconnect, just repaint — and notifications arrive from every
  environment while you are in another one.
  Cost is deliberately bounded: a background environment holds **rows, presence and
  the link cache only** — no chat docs, no transcripts, no terminals. Transcripts stay
  lazy per open chat, so "connected" costs a few hundred KB per environment, not a
  runtime.
- **Notifications carry their environment.** The row, the sound and the desktop
  notification name the environment (`VPS ovh · session terminée`), and the switcher
  shows a per-environment unread dot, so a background event is never ambiguous and
  never reads as local. One sound per burst across environments — a busy VPS must not
  become a machine gun.
- **Per-environment state**: tabs, selected space, selected chat and scroll position
  are remembered per environment in `ui-settings.json`; switching swaps the whole set,
  so returning to an environment restores where you were. Nothing is copied between
  them.
- **Switching** clears chats/spaces/devices/sessions/transcript until that
  environment's first frame is painted (`chats_synced = false`), so a stale row can
  never appear under the new label. A warm environment usually has its frame already,
  which makes the common switch instant.
- **Settings → Environments**: list with status (reachable, device count, version),
  Add, Rename, Remove (with the purge checkbox), and a copy-link action that re-prints
  the link from the stored record for pairing a second laptop.
- **Offline**: an unreachable environment shows as offline in the switcher, stops
  notifying, and its panes are read-only from the mirror; it never blocks boot and
  never silently falls back to the local environment.

## Security

- The token is a shared secret granting shell on that host. The link must be treated
  as a password: masked in `status`, never logged, not embedded in error messages.
- HTTPS is required for anything not on loopback/LAN; the existing plain-HTTP warning
  and `KOMET_SYNC_ALLOW_INSECURE_HTTP` escape hatch apply unchanged.
- `?env=` is authorized by the same IPC token as any local connection; the id is
  resolved through the environments map, so an unknown id is an error, never a default.
- Removing an environment removes its token from disk; a removed environment's
  credentials cannot be reused by the runtime because its `EnvRuntime` is dropped.

## Milestones

1. **Engine (M1)** — `Environment` record + `environments.json`, `EnvRuntime`, per-env
   RPC connections, `ListEnvironments`/`AddEnvironment`/`RemoveEnvironment`/
   `WatchEnvironments`, local-only method guard.
2. **Server + CLI (M2)** — `GET /env`, `komet serve` (both halves, `serve.json`,
   `--public-url`, `--print-link`, `--install-service`), link codec.
3. **UI (M3)** — switcher, settings page, paste/validate/preview, per-env view state,
   offline states.
4. **Hardening (M4)** — remove/purge, restart with a dead environment, version
   mismatch message, docs (`docs/environments.md`, README section).

## Testing

- `crates/engine/tests/environments.rs`: two in-process sync servers, two envs; a chat
  created in B never appears in A's `WatchChats` or `WatchSessions`; removing B stops its
  room clients; `AddEnvironment` with a bad token fails with a typed error and persists
  nothing; `AddEnvironment` against the profile's own server is rejected; an `?env=`
  unknown id is rejected; the same chat is never executed by two runtimes (one device id,
  one host per environment).
- `crates/sync-server/tests`: `GET /env` requires the bearer and reports the configured
  org/user.
- Link codec: round-trip, version rejection, malformed base64, oversized payload.
- UI unit: switching environments clears lists before the first frame and never
  renders rows from the previous one; a background environment's event produces exactly
  one notification, labelled with that environment, and no sound storm.
- Manual: fresh VPS → `serve` → paste on Fedora → start a session → confirm it runs on
  the VPS (`hostname` in a terminal) → stop the VPS → local workspace still fine.

## My calls (settle now, correct me while reading)

- Per-environment tabs and selection are remembered (not reset on switch).
- "This computer" is always listed first and cannot be removed.
- `RemoveEnvironment` forgets by default; purging the mirror is opt-in.
- The pairing link carries `url` + `token` only; identity comes from `GET /env`.
- `komet serve` refuses to print a link without `--public-url`.
- The laptop is a controller inside a remote environment, never a device of it.
- Session rows are scoped by the environment's registry index.
- The local store root is unchanged; remote environments live under
  `{data_dir}/environments/{id}/`.
- Adding a server whose `/env` identity matches the active profile is rejected.
- `GET /env` works on a bare `komet-sync-server` (identity persisted on first start).
- The in-process engine fallback supports environments through in-memory connections.
- Every environment stays connected in the background; background environments hold
  registry rows, presence and the relay link only — never chat docs or transcripts.
- Notifications are env-labelled and rate-limited across environments.

## Open — needs the user

Nothing outstanding. The connection-lifetime question was answered on 2026-10-27:
live background connections.

## Deferred, named

- Mobile/web client; per-environment tokens with revocation; environment-list sync
  across your own machines; running a *local* chat inside a remote environment's
  git worktree.
