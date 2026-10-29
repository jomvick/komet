# Security notes

## Sandbox is not a host boundary

Komet's sandbox settings configure the **agent CLI's own permission and sandbox mechanisms** (Codex `sandbox_mode`/`approvalPolicy`, Claude Code permission modes and settings permissions, OpenCode permission patterns, Grok `--sandbox`/`--permission-mode`, Cursor SDK `mode`+`sandboxOptions`, Antigravity `--sandbox`/`--mode plan`). They are **not** an OS-level isolation layer. The composer access chip (`Read only` / `Sandboxed` / `Full access`) maps to `SandboxLevel` for every harness; Claude/Codex/OpenCode also receive a `SandboxOptions` table.

### What the sandbox constrains

- The flags/permissions passed to each agent CLI at spawn time.
- With explicit `SandboxOptions`, komet maps your choices to native provider options instead of escalating to full access.

### What it does NOT constrain

- There is **no container or VM isolation**. Agent processes run directly on the host with your user's privileges.
- Anything the agent CLI can reach despite its own restrictions is still reachable: your files, network, credentials in your environment.
- A misbehaving or compromised CLI that ignores or mishandles its own sandboxing offers no second line of defense from komet.

Run agents on machines/data you accept exposing to them. Use OS-level isolation (containers, VMs) if you need a real boundary.

### Precedence rule (`options wins`)

- If a run request carries `sandbox_options`, it takes precedence over the legacy global `sandbox` level; the legacy field is ignored for that run.
- Without `sandbox_options`, behavior falls back to the legacy `sandbox` level exactly as before — old clients keep working unchanged.
- Yolo/auto-approve mode does not silently override an explicit `SandboxOptions`: granular options stay in effect even when auto-approve is on.

### Fail-fast validation

Explicit `sandbox_options` are validated by the engine (`validate_run_request`) before any process spawns. Invalid combinations (e.g. writable roots outside the workspace without full access, an OpenCode bash pattern table lacking a `"*"` fallback, unknown permissions) fail immediately with a structured error rather than producing a partially-applied configuration at runtime.

### Known limitations

- **Claude Code:** some granular constraints cannot be expressed via command-line invocation alone (e.g. strict `fail_if_unavailable` enforcement); they rely on the CLI honoring its own settings.
  - Filesystem deny rules map as `Edit(path)` (deny write) and `Read(path)` (Paseo `denyRead`, e.g. `~/.ssh`, `~/.aws`); `allow*` entries and `additionalDirectories` merge into `additionalDirectories`.
   - `sandbox.enabled` and `network.strictAllowlist` ARE first-class komet fields now (`ClaudeSandbox.enabled: Option<bool>`, `NetworkSandbox.strict_allowlist: Option<bool>`); `enabled: false` turns the whole Claude sandbox translation off, `strictAllowlist` is forwarded to the settings sandbox `network.strictAllowlist` key.
    - **Filesystem enforcement is Bash-only:** `excluded_commands` maps to `Bash(cmd:*)`, `filesystem.deny`/`deny_read` map to `Edit(path)`/`Read(path)`, and `filesystem.allow` maps to `additionalDirectories`. The CLI enforces these on tool calls, not as an OS-level filesystem jail. A `Bash` tool invocation that bypasses the permission layer is not blocked by these rules.
   - **A5 sandbox-runtime (`SandboxEnvironment::Srt`) remains opt-in only** for unattended runs — not the default. No container/VM isolation is implied by the settings above.
  - The `settingsPermissions` passthrough merges last over the generated permissions map, so it can override sandbox-derived entries. Escalation-shaped values (a `defaultMode` other than `"default"`/`"acceptEdits"`, or an allow entry of `"*"`, bare `"Bash"`, or `"Bash(*…)"`) are rejected at validation time; other passthrough content is forwarded unvalidated.
- **Codex:** `approval_policy` now expresses every Paseo level (`never`/`untrusted`/`on-request`/`granular`). `web_search` is Paseo's `disabled|cached|indexed|live` enum with bool compatibility (`true`→`live`, `false`→`disabled`); the Codex wire only carries a bool, so anything non-`disabled` maps to `webSearch: true`. `features` is Paseo's object form (`name → true|false|policy object`); the mapper emits the object so `network_proxy`-style policies survive (input arrays of names are accepted for compatibility).
- **Grok / Hermes / Pi:** the access chip is native `SandboxLevel`. Grok gets `--sandbox read-only|workspace` plus `--permission-mode plan|acceptEdits|bypassPermissions` (and `--always-approve` on full access). Hermes and Pi have no CLI sandbox; ACP `session/request_permission` auto-denies writes/commands in Read only, auto-allows on Full access, and blocks on the permission bridge when Sandboxed.
- **Cursor:** `SandboxLevel` is forwarded to the SDK shim. Read only → `mode: "plan"` + `sandboxOptions.enabled`; Sandboxed → sandbox enabled in agent mode; Full access leaves both off. (`autoReview` is classifier auto-approve and is not used for Read only.)
- **Antigravity:** Read only → `--sandbox --mode plan`; Sandboxed → `--sandbox`; Full access omits both. Print mode still needs `--dangerously-skip-permissions` because agy cannot prompt over stdio.
- **OpenCode:** the access chip always produces an `OPENCODE_CONFIG_CONTENT` overlay (`SandboxOptions.opencode` when set, otherwise `from_level(sandbox)`). Komet generates a minimal `opencode.json` and passes its CONTENT through the env var (final-precedence), so the user's own config is not touched. The bare per-tool fields `read`, `edit`, `external_directory`, `webfetch`, `websearch` are emitted alongside the `bash` pattern map; a bare field set AND present in `unscoped_actions` wins (no duplicate key). **Merge semantics validated live against opencode CLI 1.18.23** (`opencode debug config`, 2026-08-28): (1) `OPENCODE_CONFIG_CONTENT` wins over a project `opencode.json` for the `"*"` fallback — the old `OPENCODE_CONFIG` file mechanism LOST to project config (a project `allow` overrode the overlay's `ask`, silently weakening it); (2) the merge is deep — project-specific pattern entries (e.g. `git commit: allow`) survive the overlay, so komet's AllowAlways pattern updates still compose with user config. Residual: a live behavioral check that an `ask` pattern actually prompts through the ACP session (config-level precedence is verified; the ACP permission-bridge covers the runtime side and is integration-tested against the harness fixture).
- **Sandbox `ask` (Paseo parity):** when an agent requests a tool permission at runtime (`session/request_permission`), komet surfaces it as a blocking `PermissionRequested` panel and only answers the agent after the user's decision; Deny also interrupts the run. Harnesses without an interactive permission surface (and decision races with a dead run) degrade to **Deny/cancelled — never a silent allow**.

### External MCP integrations

- Composio uses its current Platform REST API v3.1 session endpoints from the Rust engine; the official SDKs are Python and TypeScript, with no Rust SDK. Each user supplies a Composio Platform project API key. Komet validates it, stores it only in the device OS credential store, and scopes Composio sessions to the stable Komet device ID. The key is never serialized into sync documents, the MCP secret file, agent process arguments, logs, or RPC replies.
- Composio's connected-account credentials remain hosted by Composio and are scoped to that device ID; removing the local API key does not delete those remote accounts. OAuth authorization happens in the user's browser through a Composio Connect Link.
- Agent tool calls are exposed through Komet's per-run local MCP endpoint and Composio session execution API, not injected as a direct hosted Composio MCP connection. Komet disables Composio's connection manager, workbench, proxy execution, and Instant usage for these sessions. Known read-only action slugs can run without a prompt; mutating and unrecognized actions require Komet's permission panel for each call. A multi-execute batch is classified as read-only only when every discovered tool slug is read-only; otherwise the entire batch requires approval.
- Other external MCP actions are not mediated by Komet's permission bridge, so per-action confirmation is not enforced for those servers. Review their tool calls and only connect accounts whose actions are appropriate for the selected agent.

## Remote environments (paired machines)

An environment is a workspace backed by another machine: `komet serve` runs a sync
server and an engine there, and a `komet://pair/…` link adds it to this desktop's
sidebar. The desktop is only a controller in that workspace, never a device of it
(`docs/environments.md`). The sandbox rules above apply unchanged on the remote host:
they constrain the agent CLI, not the machine.

### The pairing link is a shell grant

- The link is `komet://pair/` followed by unpadded base64url JSON
  `{"v":1,"label","url","token"}`. It is neither encrypted nor signed, so anyone who
  sees it can decode the token. Decoding validates only the version, non-empty fields,
  and an `http(s)` URL — there is no identity, scope, or expiry in the payload.
- Holding the link is equivalent to shell on that host: adding the environment opens
  its RPC surface through the relay — sessions, terminals, workspace file reads and
  writes, repo and harness management — as the user running `komet serve`.
- Treat it as a password. Do not paste it into chat, tickets, screenshots, or shared
  terminals; Komet does not log the link or its token, but clipboard managers and shell
  scrollback are outside Komet's control.
- `komet serve` prints it only on the run that creates or changes it, with
  `--print-link`, or with `--install-service`; a plain restart stays silent.
  **Copy link** in Settings re-encodes the stored record with the same token, so it
  produces the same secret, not a fresh one.
- There is no revoke or rotate command, endpoint, or per-link expiry, and removing an
  environment deletes only this controller's copy: the remote token stays valid and
  anyone still holding the link can keep pairing. To invalidate a leaked token you
  must replace `token` in `~/.komet/serve.json`, restart the service
  (`komet daemon restart`), and re-pair every controller with the reprinted link —
  the server then accepts only the new value, but all existing pairings break at once.

### One shared bearer per server

- Each environment has exactly one static bearer, generated into `serve.json` (0600)
  or supplied as `KOMET_SYNC_TOKEN`. Every client of that server uses the same value;
  there are no per-device or per-environment tokens, scopes, or revocation. That is a
  named non-goal of the environments design (per-environment tokens are deferred).
- The server compares that one value in constant time, from
  `Authorization: Bearer …`, `x-komet-token`, or `?token=` on WebSocket upgrades.
  A valid token authorizes any room and the device relay; there is no per-room ACL.
- On the controller, the token lives in `{data_dir}/environments.json` (0600, written
  temp-file + rename). Listings and every environment reply are redacted to an empty
  token; the one deliberate exception is the local-only `EnvironmentLink` call that
  backs **Copy link**, which is never logged or embedded in an error message.
- Rotating the token means editing `serve.json` and restarting: it affects every
  client of that server. If `environments.json` cannot be read, Komet boots with no
  environments and refuses writes rather than overwriting the stored tokens.

### Transport

- Plain HTTP is accepted without a prompt only for loopback (`localhost`, `127.0.0.1`,
  `::1`). A non-loopback `http://` link is refused by the pairing wizard until the
  plain-HTTP acknowledgement toggle is on (`needs_insecure_consent`); that consent
  gate lives in the UI, and the engine stores whatever URL the link carries.
- `komet serve --lan` fills in this machine's private address, binds the server on
  `0.0.0.0` (every interface, not just that address), and prints a plain-HTTP warning
  next to the link. Anything else is expected to sit behind a TLS reverse proxy; the
  controller opens every room and relay connection against that URL, presenting the
  token.
- `warn_if_insecure_sync_url` is informational: it logs one warning when the client
  resolves its sync URL at startup, and `KOMET_SYNC_ALLOW_INSECURE_HTTP=1` only
  changes the wording. It does not block a connection. The environment pairing path
  is gated by the UI consent above, not by that warning.
- TLS protects the transit only. Certificate validation is the HTTP client's default;
  Komet does not pin the server certificate. The token rides in the WebSocket query
  string, so it lands in reverse-proxy access logs unless logging for `/` is off; the
  proxy and the server operator also see traffic after TLS termination. Nothing here
  protects a token pasted into the wrong place.

### The device relay

- Controllers drive an environment's engine over `/device/{deviceId}/ws`: the remote
  engine claims the host role, each controller connects as a client with a connection
  id, and the server routes frames between them. Device rooms live in memory only —
  nothing is persisted or replayed. (Document rooms — `/session`, `/chat2`,
  `/registry` — are the durable ones, by design.)
- The relay decodes only the frame header to route; payloads are opaque bytes. There
  is no end-to-end encryption: TLS ends at the reverse proxy in front of the sync
  server, which receives plaintext frames, so the hub process can read every RPC
  frame, and sees device ids, connection ids, frame sizes and timing.
- One host per room. A new `role=host` socket supersedes the previous one, which is
  closed; the real engine reconnects and supersedes back. Combined with the shared
  bearer and the absent per-room ACL, any token holder can claim a room's host role
  and receive what controllers send to it.
- Disconnects fail fast instead of hanging: a client frame with no host gets
  `host_offline`, a host frame addressed to a departed client gets `client_gone`, and
  open peers get `host_closed` / `client_closed`. Keepalive `ping`/`pong` are answered
  by the server and never forwarded.

### Trust boundary on the remote machine

- `komet serve` runs an engine on that host as the user running it (for example a
  systemd `--user` or launchd service). Sessions, terminals, and the agent CLIs they
  spawn execute there with that user's privileges, on that user's files and network.
  There is no container, VM, or OS-level confinement from Komet — see
  [Sandbox is not a host boundary](#sandbox-is-not-a-host-boundary).
- The pairing grants the same RPC surface a local UI has, with no read-only mode.
  Anyone with the link can start work on that machine, not just watch it; run it under
  an account and data set you accept exposing to whoever holds the link.
- The controller is not a device inside the environment: its runtime is opened with
  `register_device = false`, so it registers no device row and appears in no device
  list there. It does keep a local mirror under `{data_dir}/environments/{id}`.
- Environments are isolated by the network boundary: each has its own server, token,
  registry and docs store, and adding the server this profile is already synced to is
  rejected, so two environments' rows cannot meet in one list.

### Unpairing and data

- **Remove** forgets the environment: its entry and token are deleted from
  `environments.json` and its sockets are torn down. The mirror under
  `{data_dir}/environments/{id}` is kept unless the purge checkbox is ticked; purge
  deletes that directory.
- The mirror can be the only copy of a remote chat — the server holds the live copy,
  and the remote machine may be gone. Export or copy it before purging.
- Removing on the controller does not stop the remote service, rotate its token, or
  delete its data. Anyone with the link can still pair; the remote keeps executing
  as before. To retire a machine, follow the teardown in `docs/environments.md`
  (disable the service, remove its data directory), and remove the desktop side
  first if you want to keep the mirror.
